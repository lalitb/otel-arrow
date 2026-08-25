// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Durability tests for the file-backed checkpoint store.
//!
//! These tests exercise the store against a real temporary directory: they
//! create and reopen namespaces, corrupt files on disk, inject a failure at
//! every persistence boundary of generation creation and compaction, and
//! assert what recovery selects afterwards.
//!
//! Two conventions matter throughout. First, a store must be dropped before
//! the same namespace is reopened, because the ownership lock is exclusive
//! even within one process. Second, an injected fault is armed on a single
//! store instance at `open` time and fires once, so a test that wants to
//! fault compaction opens an existing namespace (where `open` itself
//! publishes nothing) with the point already armed.

use std::collections::HashSet;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::super::primitives::{
    ADVISORY_PATH_MAX_BYTES, FINGERPRINT_MAX_BYTES, FileId, FramingResume, LifecycleState, Locator,
    NAMESPACE_ID_MAX_BYTES, REASON_CODE_RESERVED, TRUNCATE_RESET_REASON_READ_NEW,
    WAL_MAX_OPS_PER_TX,
};
use super::super::snapshot::{SnapshotRecord, encode_snapshot};
use super::super::wal::{
    Operation, QuarantineFile, RegisterFile, RemoveFile, ResetAfterTruncate, ResetQuarantineAction,
    ResetQuarantinedFile, Transaction, UpdateProgress, encode_wal,
};
use super::error::StoreError;
use super::fault::FaultPoint;
use super::layout::{
    CURRENT_FILE_NAME, MAX_GENERATIONS_ON_DISK, MAX_TEMP_FILES, OWNERSHIP_LOCK_FILE_NAME,
    snapshot_file_name, temp_file_name, wal_file_name,
};
use super::limits::{ARTIFACT_BYTES_CEILING, RECOVERY_WORKING_BYTES_CEILING, StoreLimits};
use super::{CheckpointStore, StoreOptions};
use crate::receivers::filelog_receiver::config::{
    CheckpointConfig, Config, IdentityConfig, LimitsConfig, RuntimeConfig,
};

const NAMESPACE_ID: &str = "filelog-test-namespace";
/// Long enough that no test can cross it while running, so an
/// interval-governed sync is deterministically deferred.
const NEVER_ELAPSES: Duration = Duration::from_secs(3600);

fn options(dir: &Path) -> StoreOptions {
    let mut options = StoreOptions::new(dir.to_path_buf(), NAMESPACE_ID.to_owned());
    options.ownership_timeout = Duration::from_millis(200);
    options.ownership_retry_interval = Duration::from_millis(10);
    options
}

fn largest_accepted(mut low: u64, mut high: u64, mut accepts: impl FnMut(u64) -> bool) -> u64 {
    assert!(accepts(low), "the lower boundary must be accepted");
    while low < high {
        let middle = low + ((high - low) / 2) + 1;
        if accepts(middle) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    low
}

fn peak_resident_set_bytes() -> Option<u64> {
    memory_stats::memory_stats().and_then(|stats| u64::try_from(stats.physical_mem).ok())
}

/// Scenario: one deferred progress transaction is followed by an idle
/// period with no later append.
/// Guarantees: the store exposes its pending deadline and the worker-facing
/// due-sync operation flushes at that deadline, so `sync_interval` is a
/// maximum duplicate window rather than an append-triggered heuristic.
#[test]
fn interval_sync_can_be_driven_while_the_source_is_idle() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = CheckpointStore::open(StoreOptions {
        sync_interval: NEVER_ELAPSES,
        ..options(&path)
    })
    .expect("namespace opens");
    let _registered = store
        .register_files(vec![registration(1)])
        .expect("registers");
    let _progressed = store
        .commit_progress(vec![progress(1, 0, 128)])
        .expect("progress succeeds");
    assert!(store.next_sync_deadline().is_some());
    assert!(!store.sync_if_due().expect("the early poll succeeds"));

    store.last_sync = Instant::now() - NEVER_ELAPSES;
    assert!(store.sync_if_due().expect("the deadline poll syncs"));
    assert!(store.next_sync_deadline().is_none());
    assert_eq!(store.stats().unsynced_transactions, 0);
}

/// Scenario: `checkpoint.sync_interval` is `Duration::MAX`, which cannot be
/// represented as an `Instant` deadline on supported platforms.
/// Guarantees: deadline arithmetic never panics or wraps; the store
/// conservatively syncs Ack-driven progress immediately and leaves no
/// outstanding deadline to busy-loop on.
#[test]
fn unrepresentable_sync_interval_is_immediately_due() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = CheckpointStore::open(StoreOptions {
        sync_interval: Duration::MAX,
        ..options(&path)
    })
    .expect("namespace opens");
    let _registered = store
        .register_files(vec![registration(1)])
        .expect("registers");

    let outcome = store
        .commit_progress(vec![progress(1, 0, 128)])
        .expect("progress succeeds");
    assert!(outcome[0].synced);
    assert_eq!(store.stats().unsynced_transactions, 0);
    assert!(store.next_sync_deadline().is_none());
}

fn open(dir: &Path) -> CheckpointStore {
    CheckpointStore::open(options(dir)).expect("namespace opens")
}

/// Scenario: `CURRENT` is missing and the only surviving half of initial
/// generation 0 declares another generation, or is a WAL with a torn tail.
/// Guarantees: only an exact, complete, empty generation-0 artifact is
/// treated as interrupted first creation; foreign or torn survivors fail
/// closed instead of being overwritten by empty state.
#[test]
fn incomplete_initial_generation_rejects_foreign_or_torn_survivors() {
    let dir = tempfile::tempdir().expect("temp dir");

    let foreign = dir.path().join("foreign");
    drop(open(&foreign));
    fs::remove_file(foreign.join(CURRENT_FILE_NAME)).expect("removes the marker");
    fs::remove_file(foreign.join(wal_file_name(0))).expect("removes the WAL");
    write_bytes(
        &foreign.join(snapshot_file_name(0)),
        &encode_snapshot(9, &[]).expect("foreign snapshot encodes"),
    );
    assert!(matches!(
        CheckpointStore::open(options(&foreign)).expect_err("foreign generation fails closed"),
        StoreError::GenerationMismatch {
            artifact: "snapshot",
            expected: 0,
            found: 9,
            ..
        }
    ));

    let torn = dir.path().join("torn");
    drop(open(&torn));
    fs::remove_file(torn.join(CURRENT_FILE_NAME)).expect("removes the marker");
    fs::remove_file(torn.join(snapshot_file_name(0))).expect("removes the snapshot");
    let mut wal = fs::OpenOptions::new()
        .append(true)
        .open(torn.join(wal_file_name(0)))
        .expect("WAL opens");
    wal.write_all(&[0, 0, 1]).expect("torn tail appends");
    drop(wal);
    assert!(matches!(
        CheckpointStore::open(options(&torn)).expect_err("torn survivor fails closed"),
        StoreError::IncompleteInitialGeneration {
            reason: "its surviving WAL has a torn tail",
            ..
        }
    ));
}

fn file_id(seed: u8) -> FileId {
    FileId([seed; 16])
}

/// Scenario: old records include one active file that the runtime still
/// owns, one absent finalized file, and one absent quarantine.
/// Guarantees: age alone never makes a record removable; the caller-vetted
/// absent/open/in-flight set gates retention, and quarantine remains immune
/// even when it is in that set.
#[test]
fn retention_requires_runtime_vetted_absence() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = open(&path);
    let _registered = store
        .register_files(vec![registration(1), registration(2), registration(3)])
        .expect("registers");
    let _quarantined = store
        .quarantine_files(vec![quarantine(2)])
        .expect("quarantines");
    let mut finalize = progress(3, 0, 64);
    finalize.finalize = true;
    let _finalized = store.commit_progress(vec![finalize]).expect("finalizes");

    let eligible_absent = HashSet::from([file_id(2), file_id(3)]);
    let candidates =
        store.retention_candidates(&eligible_absent, 10_000_000_000, Duration::from_secs(1));
    assert_eq!(candidates, vec![file_id(3)]);
}

/// Scenario: a quarantine-only administrative removal targets an active
/// record, including one that was quarantined and then reset to active.
/// Guarantees: a stale operator command cannot delete live checkpoint state
/// after quarantine release.
#[test]
fn quarantine_removal_rejects_a_record_that_is_now_active() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = open(&path);
    let _registered = store
        .register_files(vec![registration(1)])
        .expect("registers");
    assert!(matches!(
        store
            .remove_quarantined_file(file_id(1), 1, 1, "stale purge".to_owned())
            .expect_err("active state is refused"),
        StoreError::NotQuarantined {
            state: LifecycleState::Active,
            ..
        }
    ));

    let _quarantined = store
        .quarantine_files(vec![quarantine(1)])
        .expect("quarantines");
    assert_eq!(store.stats().quarantined_records, 1);
    let _reset = store
        .reset_quarantined_file(ResetQuarantinedFile {
            file_id: file_id(1),
            expected_quarantine_epoch: 1,
            action: ResetQuarantineAction::ResetToBeginning,
            resulting_epoch: 2,
            resulting_offset: 0,
            new_framing_resume: FramingResume::Clean,
            reset_time_unix_nano: 4_000,
            audit_reason: "release".to_owned(),
        })
        .expect("resets");
    assert_eq!(store.stats().quarantined_records, 0);
    assert!(matches!(
        store
            .remove_quarantined_file(file_id(1), 1, 2, "stale purge".to_owned())
            .expect_err("reset active state is refused"),
        StoreError::NotQuarantined {
            state: LifecycleState::Active,
            ..
        }
    ));
    assert!(store.table().get(&file_id(1)).is_some());
}

fn registration(seed: u8) -> RegisterFile {
    RegisterFile {
        file_id: file_id(seed),
        file_epoch: 1,
        committed_offset: 0,
        fingerprint: vec![seed; 8],
        ignored_header_bytes: 0,
        locator: Locator::PosixDevIno {
            dev: 7,
            ino: u64::from(seed),
        },
        framing_profile_version: 1,
        framing_profile_digest: [0x11; 32],
        framing_resume: FramingResume::Clean,
        last_seen_time_unix_nano: 1_000,
        advisory_path: format!("/var/log/app-{seed}.log").into_bytes(),
    }
}

fn distinct_registrations(count: usize) -> Vec<RegisterFile> {
    (0..count)
        .map(|index| RegisterFile {
            file_id: wide_file_id(index as u64),
            ..registration(1)
        })
        .collect()
}

/// Scenario: a nearly full WAL transaction is followed by a two-operation
/// caller-defined atomic group.
/// Guarantees: grouped packing starts a new transaction before the pair
/// instead of splitting the pair at the format operation boundary.
#[test]
fn atomic_group_packing_never_splits_a_group() {
    let operation = Operation::RegisterFile(registration(1));
    let leading = vec![operation.clone(); usize::from(WAL_MAX_OPS_PER_TX) - 1];
    let pair = vec![operation.clone(), operation];

    let (operations, transaction_lengths) =
        super::pack_atomic_groups(vec![leading, pair]).expect("groups pack");

    assert_eq!(operations.len(), usize::from(WAL_MAX_OPS_PER_TX) + 1);
    assert_eq!(
        transaction_lengths,
        vec![usize::from(WAL_MAX_OPS_PER_TX) - 1, 2]
    );
}

/// Scenario: 4,095 valid registrations fill the first grouped WAL
/// transaction and a `register_file + quarantine_file` pair starts the
/// second transaction, where every WAL persistence boundary is faulted.
/// Guarantees: actual packing, append, and recovery preserve the pair as an
/// indivisible group at the transaction boundary; reopen sees it either
/// absent or fully quarantined, never active.
#[test]
fn atomic_group_boundary_survives_every_wal_fault() {
    for point in FaultPoint::WAL_DURABILITY {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("namespace");
        let mut store_options = options(&path);
        store_options.max_tracked_files = u32::from(WAL_MAX_OPS_PER_TX);
        drop(CheckpointStore::open(store_options.clone()).expect("namespace initializes"));

        let mut registrations = distinct_registrations(usize::from(WAL_MAX_OPS_PER_TX));
        let candidate = registrations.pop().expect("candidate registration");
        let candidate_id = candidate.file_id;
        let candidate_locator = candidate.locator;
        let mut groups: Vec<Vec<Operation>> = registrations
            .into_iter()
            .map(|registration| vec![Operation::RegisterFile(registration)])
            .collect();
        groups.push(vec![
            Operation::RegisterFile(candidate),
            Operation::QuarantineFile(QuarantineFile {
                file_id: candidate_id,
                expected_file_epoch: 1,
                reason_code: 0x0003,
                locator: candidate_locator,
                observed_size: 8,
                quarantine_epoch: 1,
                quarantine_time_unix_nano: 2_000,
            }),
        ]);

        let mut faulted = CheckpointStore::open_with_fault_after(store_options.clone(), point, 1)
            .expect("faulted store opens");
        assert!(
            faulted.append_atomic_groups(groups).is_err(),
            "{point:?} must interrupt the second transaction"
        );
        drop(faulted);

        let recovered = CheckpointStore::open(store_options).expect("namespace recovers");
        if let Some(record) = recovered.table().get(&candidate_id) {
            assert_eq!(
                record.lifecycle_state,
                LifecycleState::Quarantined,
                "{point:?} recovered a register-only candidate"
            );
        }
    }
}

/// Scenario: one Ack delta set exceeds the format's operation maximum, and
/// the next sequence is forced to the final `u64` value.
/// Guarantees: progress is one WAL transaction and both bounds are
/// preflighted before table or WAL mutation; no partial Ack prefix or
/// unrecoverable final sequence can become durable.
#[test]
fn progress_atomicity_and_sequence_are_preflighted_before_mutation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = open(&path);
    let _registered = store
        .register_files(vec![registration(1)])
        .expect("registers");

    let bytes_before = store.stats().wal_bytes;
    let sequence_before = store.stats().next_sequence;
    let oversized = vec![progress(1, 0, 1); usize::from(WAL_MAX_OPS_PER_TX) + 1];
    assert!(matches!(
        store
            .commit_progress(oversized)
            .expect_err("an oversized Ack delta is refused atomically"),
        StoreError::TransactionTooLarge { .. }
    ));
    assert_eq!(committed_offset(&store, 1), 0);
    assert_eq!(store.stats().wal_bytes, bytes_before);
    assert_eq!(store.stats().next_sequence, sequence_before);

    store.next_sequence = u64::MAX;
    assert!(matches!(
        store
            .commit_progress(vec![progress(1, 0, 1)])
            .expect_err("the final sequence is refused before writing"),
        StoreError::SequenceOverflow { sequence: u64::MAX }
    ));
    assert_eq!(committed_offset(&store, 1), 0);
    assert_eq!(store.stats().wal_bytes, bytes_before);
}

/// Scenario: append accounting counters are forced to their final `u64`
/// values before an otherwise valid progress update.
/// Guarantees: each counter overflow is reported before the table, WAL, or
/// sequence changes, rather than wrapping or making a partially persisted
/// transaction observable.
#[test]
fn append_counter_overflow_is_preflighted_before_mutation() {
    for counter in ["wal_transactions", "unsynced_transactions", "syncs"] {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("namespace");
        let mut store = CheckpointStore::open(StoreOptions {
            sync_interval: NEVER_ELAPSES,
            ..options(&path)
        })
        .expect("namespace opens");
        let _registered = store
            .register_files(vec![registration(1)])
            .expect("registers");
        match counter {
            "wal_transactions" => store.wal_transactions = u64::MAX,
            "unsynced_transactions" => store.unsynced_transactions = u64::MAX,
            "syncs" => store.syncs = u64::MAX,
            other => panic!("unexpected counter {other}"),
        }
        let bytes_before = store.wal_bytes;
        let sequence_before = store.next_sequence;

        assert!(matches!(
            store
                .commit_progress(vec![progress(1, 0, 1)])
                .expect_err("counter overflow is refused"),
            StoreError::CounterOverflow { .. }
        ));
        assert_eq!(committed_offset(&store, 1), 0);
        assert_eq!(store.wal_bytes, bytes_before);
        assert_eq!(store.next_sequence, sequence_before);
    }

    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("batched");
    let count = usize::from(WAL_MAX_OPS_PER_TX) + 1;
    let mut store = CheckpointStore::open(StoreOptions {
        max_tracked_files: count as u32,
        ..options(&path)
    })
    .expect("namespace opens");
    store.syncs = u64::MAX - 1;
    let before = store.stats();
    assert!(matches!(
        store
            .register_files(distinct_registrations(count))
            .expect_err("all chunk sync counters are preflighted"),
        StoreError::CounterOverflow {
            counter: "WAL syncs",
            ..
        }
    ));
    assert!(store.table().is_empty());
    assert_eq!(store.wal_bytes, before.wal_bytes);
    assert_eq!(store.next_sequence, before.next_sequence);
}

/// Scenario: an existing Unix namespace and ownership lock have permissive
/// modes, and deterministic temporary names are symlinked to a victim file.
/// Guarantees: opening repairs private modes, temporary publication unlinks
/// the symlink itself without touching its target, and a symlinked lock file
/// is rejected rather than followed.
#[cfg(unix)]
#[test]
fn unix_permissions_and_symlink_targets_are_safe() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    fs::create_dir(&path).expect("namespace is precreated");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).expect("mode changes");
    let lock_path = path.join(OWNERSHIP_LOCK_FILE_NAME);
    write_bytes(&lock_path, b"");
    fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o666)).expect("mode changes");

    let mut store = open(&path);
    assert_eq!(
        fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&lock_path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let victim = dir.path().join("victim");
    write_bytes(&victim, b"must survive");
    let marker_temp = path.join(format!("{CURRENT_FILE_NAME}.tmp"));
    symlink(&victim, &marker_temp).expect("temporary symlink is planted");
    store
        .compact()
        .expect("publication safely replaces the temp name");
    assert_eq!(fs::read(&victim).expect("victim reads"), b"must survive");
    drop(store);

    fs::remove_file(&lock_path).expect("real lock is removed");
    symlink(&victim, &lock_path).expect("lock symlink is planted");
    assert!(CheckpointStore::open(options(&path)).is_err());
    assert_eq!(fs::read(&victim).expect("victim reads"), b"must survive");
}

/// Scenario: an authoritative checkpoint WAL acquires a second hard link
/// before the namespace is reopened.
/// Guarantees: handle-based link-count validation rejects the artifact on
/// both Unix and Windows, preventing appends or truncation through a name
/// that aliases another filesystem location.
#[test]
fn hard_linked_checkpoint_artifacts_are_rejected() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    drop(open(&path));
    let wal_path = path.join(wal_file_name(0));
    let alias_path = dir.path().join("wal-alias");
    fs::hard_link(&wal_path, &alias_path).expect("hard link is created");

    let error = CheckpointStore::open(options(&path)).expect_err("the hard link is rejected");
    assert!(matches!(
        error,
        StoreError::UnsafeFilesystemObject { path: rejected, .. } if rejected == wal_path
    ));
}

/// Scenario: a Windows checkpoint namespace path is a directory reparse
/// point rather than a real directory.
/// Guarantees: opening rejects the reparse point before creating or
/// modifying any checkpoint artifact through it.
#[cfg(windows)]
#[test]
fn windows_reparse_point_namespace_is_rejected() {
    use std::os::windows::fs::{symlink_dir, symlink_file};

    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("target");
    fs::create_dir(&target).expect("target directory is created");
    let link = dir.path().join("namespace");
    symlink_dir(&target, &link).expect("directory symlink is created");

    let error = CheckpointStore::open(options(&link)).expect_err("the reparse point is rejected");
    assert!(matches!(error, StoreError::UnsafeFilesystemObject { .. }));
    assert!(
        fs::read_dir(&target)
            .expect("target directory is readable")
            .next()
            .is_none()
    );

    let artifact_namespace = dir.path().join("artifact-namespace");
    drop(open(&artifact_namespace));
    let wal_path = artifact_namespace.join(wal_file_name(0));
    let wal_target = dir.path().join("wal-target");
    fs::rename(&wal_path, &wal_target).expect("the original WAL is preserved");
    symlink_file(&wal_target, &wal_path).expect("file symlink is created");
    let error = CheckpointStore::open(options(&artifact_namespace))
        .expect_err("the artifact reparse point is rejected");
    assert!(matches!(error, StoreError::UnsafeFilesystemObject { .. }));
    assert_eq!(
        fs::read(&wal_target).expect("the target WAL remains readable"),
        encode_wal(0, &[]).expect("the empty WAL encodes")
    );
}

fn progress(seed: u8, from: u64, to: u64) -> UpdateProgress {
    UpdateProgress {
        file_id: file_id(seed),
        expected_committed_offset: from,
        expected_file_epoch: 1,
        new_committed_offset: to,
        new_framing_resume: FramingResume::Clean,
        new_last_seen_time_unix_nano: 2_000,
        finalize: false,
    }
}

/// Scenario: one immediate-durability registration is appended to a newly
/// opened checkpoint store.
/// Guarantees: byte, transaction, persist-operation, sync-operation, and
/// namespace-lock telemetry sources advance once without changing durable
/// transition behavior.
#[test]
fn store_stats_expose_authoritative_persistence_telemetry() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = CheckpointStore::open(options(directory.path())).unwrap();
    let before = store.stats();
    let outcome = store
        .append(vec![Operation::RegisterFile(registration(1))])
        .unwrap();
    let after = store.stats();

    assert_eq!(after.wal_bytes_appended, outcome.bytes);
    assert_eq!(after.transactions_appended, 1);
    assert_eq!(after.persist_operations, 1);
    assert_eq!(after.syncs, before.syncs + 1);
    assert_eq!(after.sync_operations, 1);
    assert!(after.namespace_lock_wait_ns >= before.namespace_lock_wait_ns);
}

fn quarantine(seed: u8) -> QuarantineFile {
    QuarantineFile {
        file_id: file_id(seed),
        expected_file_epoch: 1,
        reason_code: 0x0001,
        locator: Locator::PosixDevIno {
            dev: 7,
            ino: u64::from(seed),
        },
        observed_size: 512,
        quarantine_epoch: 1,
        quarantine_time_unix_nano: 3_000,
    }
}

fn committed_offset(store: &CheckpointStore, seed: u8) -> u64 {
    store
        .table()
        .get(&file_id(seed))
        .expect("record is tracked")
        .committed_offset
}

fn records(store: &CheckpointStore) -> Vec<SnapshotRecord> {
    store.table().snapshot_records()
}

fn write_bytes(path: &PathBuf, bytes: &[u8]) {
    fs::write(path, bytes).expect("test fixture write succeeds");
}

/// Grows `path` to `len` bytes without writing them. The store checks a
/// file's length before it allocates a buffer for it, so a sparse file is
/// enough to exercise the oversized-artifact refusal without spending the
/// bytes.
fn grow_file(path: &PathBuf, len: u64) {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("the artifact opens for growing")
        .set_len(len)
        .expect("the artifact grows");
}

/// A distinct `file_id` for the wide-record fixtures, kept clear of the
/// seeded `file_id(seed)` values.
fn wide_file_id(index: u64) -> FileId {
    let mut bytes = [0xEE; 16];
    bytes[0..8].copy_from_slice(&index.to_be_bytes());
    FileId(bytes)
}

/// A registration whose advisory path is as long as the format allows, so
/// a handful of records approach the worst case the size formulas bound.
fn widest_registration(index: u64) -> RegisterFile {
    RegisterFile {
        file_id: wide_file_id(index),
        // The widest locator variant; `register_file` must still carry the
        // clean resume state and epoch 1 that replay requires.
        locator: Locator::WindowsVolumeFileId {
            volume_serial: u64::MAX,
            file_id: [0xAB; 16],
        },
        fingerprint: vec![0x5A; 16],
        advisory_path: vec![b'p'; ADVISORY_PATH_MAX_BYTES],
        ..registration(1)
    }
}

fn wide_progress(index: u64, from: u64, to: u64) -> UpdateProgress {
    UpdateProgress {
        file_id: wide_file_id(index),
        ..progress(1, from, to)
    }
}

fn removal(seed: u8, removal_reason: u16) -> RemoveFile {
    RemoveFile {
        file_id: file_id(seed),
        expected_file_epoch: 1,
        expected_prior_state: LifecycleState::Active,
        removal_reason,
        removal_time_unix_nano: 9_000,
        administrative: false,
        namespace_id: None,
        audit_reason: None,
    }
}

/// Seeds a namespace with two registered files and progress on the first,
/// fully synced, and returns its recovered records for later comparison.
fn seeded_namespace(dir: &Path) -> Vec<SnapshotRecord> {
    let mut store = open(dir);
    let _registered = store
        .register_files(vec![registration(1), registration(2)])
        .expect("registration succeeds");
    let _progressed = store
        .commit_progress(vec![progress(1, 0, 4_096)])
        .expect("progress succeeds");
    store.drain().expect("drain syncs");
    let seeded = records(&store);
    drop(store);
    seeded
}

/// Scenario: opening a namespace directory that does not exist yet, then
/// reopening the namespace it created.
/// Guarantees: the first open creates generation 0 with an empty table, a
/// complete snapshot/WAL pair, a `CURRENT` marker, and an ownership lock;
/// the second open reports that it created nothing and recovers the same
/// generation.
#[test]
fn initial_open_creates_generation_zero_and_reopen_selects_it() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");

    let store = open(&path);
    assert!(store.recovery().created);
    assert!(!store.recovery().adopted_without_marker);
    assert_eq!(store.generation(), 0);
    assert_eq!(store.table().len(), 0);
    assert!(path.join(CURRENT_FILE_NAME).is_file());
    assert!(path.join(snapshot_file_name(0)).is_file());
    assert!(path.join(wal_file_name(0)).is_file());
    assert!(path.join(OWNERSHIP_LOCK_FILE_NAME).is_file());
    drop(store);

    let reopened = open(&path);
    assert!(!reopened.recovery().created);
    assert_eq!(reopened.generation(), 0);
    assert_eq!(reopened.recovery().transactions_replayed, 0);
    assert_eq!(reopened.stats().next_sequence, 1);
}

/// Scenario: registering files, advancing progress, then reopening the
/// namespace.
/// Guarantees: recovery replays the WAL onto the snapshot base, so every
/// registered file and its last acknowledged offset survive a restart, and
/// the next transaction continues the sequence rather than restarting it.
#[test]
fn reopen_recovers_registered_and_acknowledged_state() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let seeded = seeded_namespace(&path);

    let store = open(&path);
    assert_eq!(records(&store), seeded);
    assert_eq!(store.table().len(), 2);
    assert_eq!(committed_offset(&store, 1), 4_096);
    assert_eq!(committed_offset(&store, 2), 0);
    assert_eq!(store.recovery().snapshot_records, 0);
    assert_eq!(store.recovery().transactions_replayed, 2);
    assert_eq!(store.stats().next_sequence, 3);
}

/// Scenario: a namespace whose snapshot file declares a generation that
/// differs from the one `CURRENT` selected and from its own file name.
/// Guarantees: recovery cross-checks the marker, snapshot, and WAL
/// generations and fails closed on disagreement instead of recovering from a
/// file that belongs to a different generation.
#[test]
fn open_fails_closed_when_the_snapshot_declares_another_generation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let _seeded = seeded_namespace(&path);

    // A snapshot encoded for generation 9, stored under generation 0's name.
    let foreign = encode_snapshot(9, &[]).expect("encodes");
    write_bytes(&path.join(snapshot_file_name(0)), &foreign);

    let error = CheckpointStore::open(options(&path)).expect_err("mismatch fails closed");
    match error {
        StoreError::GenerationMismatch {
            artifact,
            expected,
            found,
            ..
        } => {
            assert_eq!(artifact, "snapshot");
            assert_eq!(expected, 0);
            assert_eq!(found, 9);
        }
        other => panic!("expected a generation mismatch, got {other:?}"),
    }
}

/// Scenario: a namespace whose WAL file declares a generation that differs
/// from the one `CURRENT` selected.
/// Guarantees: the WAL is cross-checked against the selected generation
/// exactly like the snapshot, so a stale or foreign WAL can never be
/// replayed onto the wrong snapshot base.
#[test]
fn open_fails_closed_when_the_wal_declares_another_generation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let _seeded = seeded_namespace(&path);

    let foreign = encode_wal(4, &[]).expect("encodes");
    write_bytes(&path.join(wal_file_name(0)), &foreign);

    let error = CheckpointStore::open(options(&path)).expect_err("mismatch fails closed");
    match error {
        StoreError::GenerationMismatch {
            artifact,
            expected,
            found,
            ..
        } => {
            assert_eq!(artifact, "WAL");
            assert_eq!(expected, 0);
            assert_eq!(found, 4);
        }
        other => panic!("expected a generation mismatch, got {other:?}"),
    }
}

/// Scenario: `CURRENT` selects a generation whose WAL file has been removed.
/// Guarantees: a generation the marker names is only recoverable as a
/// complete pair; a missing half fails closed instead of being recreated,
/// because the marker proves the pair was authoritative.
#[test]
fn open_fails_closed_when_the_selected_generation_is_incomplete() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let _seeded = seeded_namespace(&path);

    fs::remove_file(path.join(wal_file_name(0))).expect("removes the WAL");

    let error = CheckpointStore::open(options(&path)).expect_err("incomplete pair fails closed");
    match error {
        StoreError::IncompleteGeneration {
            generation,
            missing,
            ..
        } => {
            assert_eq!(generation, 0);
            assert_eq!(missing, "the WAL file");
        }
        other => panic!("expected an incomplete generation, got {other:?}"),
    }
}

/// Scenario: more new-file registrations than one WAL transaction may carry.
/// Guarantees: registrations are batched into as few transactions as the
/// format's per-transaction maximum allows, each one is synced before the
/// call returns, sequences stay contiguous, and every registration survives
/// a reopen.
#[test]
fn registrations_batch_into_bounded_synced_transactions() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = CheckpointStore::open(StoreOptions {
        sync_interval: NEVER_ELAPSES,
        ..options(&path)
    })
    .expect("namespace opens");

    let count = usize::from(WAL_MAX_OPS_PER_TX) + 5;
    let registrations: Vec<RegisterFile> = (0..count)
        .map(|index| {
            let mut register = registration(1);
            let mut bytes = [0u8; 16];
            bytes[0..8].copy_from_slice(&(index as u64).to_be_bytes());
            register.file_id = FileId(bytes);
            register
        })
        .collect();

    let outcomes = store.register_files(registrations).expect("registers");
    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0].operations, usize::from(WAL_MAX_OPS_PER_TX));
    assert_eq!(outcomes[1].operations, 5);
    assert_eq!(outcomes[0].sequence, 1);
    assert_eq!(outcomes[1].sequence, 2);
    // Registration must be durable before the receiver reads the file, so a
    // long sync interval must not defer it.
    assert!(outcomes.iter().all(|outcome| outcome.synced));
    assert_eq!(store.stats().unsynced_transactions, 0);
    assert_eq!(store.table().len(), count);
    drop(store);

    let reopened = open(&path);
    assert_eq!(reopened.table().len(), count);
    assert_eq!(reopened.recovery().transactions_replayed, 2);
}

/// Scenario: a filesystem failure occurs before writing the second WAL
/// transaction of a two-chunk registration batch.
/// Guarantees: the first chunk remains durable, the divergent live handle
/// becomes unusable, reopen exposes exactly the durable prefix, and retrying
/// the absent suffix completes the logical batch without duplication.
#[test]
fn filesystem_failure_between_batch_chunks_recovers_a_retryable_prefix() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let count = usize::from(WAL_MAX_OPS_PER_TX) + 1;
    let store_options = StoreOptions {
        max_tracked_files: count as u32,
        ..options(&path)
    };
    let registrations = distinct_registrations(count);
    let retry = registrations[count - 1].clone();
    let mut store = CheckpointStore::open_with_fault_after(
        store_options.clone(),
        FaultPoint::BeforeWalTransactionWrite,
        1,
    )
    .expect("namespace opens");

    let error = store
        .register_files(registrations)
        .expect_err("the second chunk faults");
    assert!(matches!(
        error,
        StoreError::InjectedFault {
            point: FaultPoint::BeforeWalTransactionWrite
        }
    ));
    assert!(matches!(
        store.sync().expect_err("the divergent handle is unusable"),
        StoreError::Unusable { .. }
    ));
    drop(store);

    let mut reopened =
        CheckpointStore::open(store_options.clone()).expect("durable prefix reopens");
    assert_eq!(reopened.table().len(), usize::from(WAL_MAX_OPS_PER_TX));
    assert_eq!(reopened.recovery().transactions_replayed, 1);
    assert!(reopened.table().get(&retry.file_id).is_none());

    let outcomes = reopened
        .register_files(vec![retry.clone()])
        .expect("the absent suffix retries");
    assert_eq!(outcomes[0].sequence, 2);
    assert_eq!(reopened.table().len(), count);
    drop(reopened);

    let complete = CheckpointStore::open(store_options).expect("completed batch reopens");
    assert_eq!(complete.table().len(), count);
    assert!(complete.table().get(&retry.file_id).is_some());
}

/// Scenario: one registration batch spans two WAL transactions and reaches
/// `max_tracked_files` exactly, while an otherwise identical batch exceeds
/// that limit by one record.
/// Guarantees: the exact-cap batch commits completely, but the cap-plus-one
/// batch is rejected before its first chunk changes the table, WAL length,
/// or sequence.
#[test]
fn batched_registration_capacity_is_preflighted_across_all_chunks() {
    let dir = tempfile::tempdir().expect("temp dir");
    let count = usize::from(WAL_MAX_OPS_PER_TX) + 1;

    let exact_path = dir.path().join("exact");
    let exact_options = StoreOptions {
        max_tracked_files: count as u32,
        ..options(&exact_path)
    };
    let mut exact = CheckpointStore::open(exact_options.clone()).expect("namespace opens");
    let outcomes = exact
        .register_files(distinct_registrations(count))
        .expect("the exact-cap batch succeeds");
    assert_eq!(outcomes.len(), 2);
    assert_eq!(exact.table().len(), count);
    drop(exact);
    assert_eq!(
        CheckpointStore::open(exact_options)
            .expect("the exact-cap namespace reopens")
            .table()
            .len(),
        count
    );

    let over_path = dir.path().join("over");
    let over_options = StoreOptions {
        max_tracked_files: WAL_MAX_OPS_PER_TX.into(),
        ..options(&over_path)
    };
    let mut over = CheckpointStore::open(over_options.clone()).expect("namespace opens");
    let before = over.stats();
    let error = over
        .register_files(distinct_registrations(count))
        .expect_err("the cap-plus-one batch is refused");
    assert!(matches!(
        error,
        StoreError::TrackedFilesExhausted {
            tracked: 0,
            registrations,
            max,
            ..
        } if registrations == count && max == u32::from(WAL_MAX_OPS_PER_TX)
    ));
    assert_eq!(over.table().len(), 0);
    assert_eq!(over.stats().wal_bytes, before.wal_bytes);
    assert_eq!(over.stats().next_sequence, before.next_sequence);
    drop(over);
    assert!(
        CheckpointStore::open(over_options)
            .expect("the rejected namespace reopens")
            .table()
            .is_empty()
    );
}

/// Scenario: a caller batch has one full valid transaction followed by a
/// conflicting registration in its second transaction.
/// Guarantees: transition validation covers the logical batch before any
/// chunk is persisted, so the late deterministic failure leaves the first
/// chunk, sequence, and table untouched.
#[test]
fn invalid_later_batched_operation_is_rejected_before_the_first_chunk() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let count = usize::from(WAL_MAX_OPS_PER_TX);
    let mut operations: Vec<Operation> = distinct_registrations(count)
        .into_iter()
        .map(Operation::RegisterFile)
        .collect();
    let mut conflicting = distinct_registrations(1).pop().expect("one registration");
    conflicting.advisory_path.push(b'x');
    operations.push(Operation::RegisterFile(conflicting));

    let mut store = CheckpointStore::open(StoreOptions {
        max_tracked_files: count as u32,
        ..options(&path)
    })
    .expect("namespace opens");
    let before = store.stats();
    let error = store
        .append_batched(operations)
        .expect_err("the later conflict is refused");
    assert!(matches!(error, StoreError::Apply { .. }));
    assert!(store.table().is_empty());
    assert_eq!(store.stats().wal_bytes, before.wal_bytes);
    assert_eq!(store.stats().next_sequence, before.next_sequence);
}

/// Scenario: a two-transaction registration batch fits the tracked-file
/// limit but only its first maximal transaction fits the configured WAL
/// budget.
/// Guarantees: WAL capacity is projected across every chunk before the
/// first write, so rejecting the second chunk cannot leave a partially
/// committed caller batch.
#[test]
fn batched_wal_capacity_is_preflighted_across_all_chunks() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let count = usize::from(WAL_MAX_OPS_PER_TX) + 128;
    let mut store = CheckpointStore::open(StoreOptions {
        compact_after_bytes: 1,
        fingerprint_bytes: 16,
        max_tracked_files: count as u32,
        ..options(&path)
    })
    .expect("namespace opens");
    let before = store.stats();
    let registrations: Vec<RegisterFile> = (0..count as u64).map(widest_registration).collect();

    let error = store
        .register_files(registrations)
        .expect_err("the complete batch exceeds the WAL cap");
    match error {
        StoreError::WalWouldExceedMaximum {
            wal_bytes,
            transaction_bytes,
            max,
            ..
        } => {
            assert!(wal_bytes > before.wal_bytes);
            assert!(wal_bytes + transaction_bytes > max);
        }
        other => panic!("expected a WAL capacity refusal, got {other:?}"),
    }
    assert!(store.table().is_empty());
    assert_eq!(store.stats().wal_bytes, before.wal_bytes);
    assert_eq!(store.stats().next_sequence, before.next_sequence);
}

/// Scenario: Ack-driven progress under a zero sync interval and under an
/// interval that cannot elapse during the test.
/// Guarantees: a zero interval syncs every progress transaction, a
/// configured interval defers the sync of progress transactions only, and a
/// deferred transaction is still written to the WAL, so widening the
/// interval widens the crash-duplicate window without ever creating a loss
/// window.
#[test]
fn progress_sync_follows_the_configured_interval() {
    let dir = tempfile::tempdir().expect("temp dir");
    let immediate_path = dir.path().join("immediate");
    let deferred_path = dir.path().join("deferred");

    let mut immediate = CheckpointStore::open(StoreOptions {
        sync_interval: Duration::ZERO,
        ..options(&immediate_path)
    })
    .expect("namespace opens");
    let _registered = immediate
        .register_files(vec![registration(1)])
        .expect("registers");
    let syncs_before = immediate.stats().syncs;
    let outcomes = immediate
        .commit_progress(vec![progress(1, 0, 128)])
        .expect("progress succeeds");
    assert!(outcomes[0].synced);
    assert_eq!(immediate.stats().syncs, syncs_before + 1);
    assert_eq!(immediate.stats().unsynced_transactions, 0);

    let mut deferred = CheckpointStore::open(StoreOptions {
        sync_interval: NEVER_ELAPSES,
        ..options(&deferred_path)
    })
    .expect("namespace opens");
    let _registered = deferred
        .register_files(vec![registration(1)])
        .expect("registers");
    let syncs_before = deferred.stats().syncs;
    let outcomes = deferred
        .commit_progress(vec![progress(1, 0, 128)])
        .expect("progress succeeds");
    assert!(!outcomes[0].synced);
    assert_eq!(deferred.stats().syncs, syncs_before);
    assert_eq!(deferred.stats().unsynced_transactions, 1);
    // The bytes are in the WAL either way: only the fsync was deferred.
    assert_eq!(committed_offset(&deferred, 1), 128);
    drop(deferred);

    let reopened = open(&deferred_path);
    assert_eq!(committed_offset(&reopened, 1), 128);
}

/// Scenario: registration, truncate reset, quarantine, quarantine reset, and
/// removal issued under a sync interval that cannot elapse during the test.
/// Guarantees: the operations whose effect must be durable before it is
/// observable are synced immediately whatever the interval is, so the
/// interval can only ever defer acknowledged-progress durability.
#[test]
fn required_operations_sync_immediately_despite_the_interval() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = CheckpointStore::open(StoreOptions {
        sync_interval: NEVER_ELAPSES,
        ..options(&path)
    })
    .expect("namespace opens");

    let outcomes = store
        .register_files(vec![registration(1), registration(2)])
        .expect("registers");
    assert!(outcomes[0].synced);

    let outcome = store
        .reset_after_truncate(ResetAfterTruncate {
            file_id: file_id(1),
            expected_active_epoch: 1,
            observed_truncated_size: 16,
            resulting_epoch: 2,
            new_committed_offset: 0,
            new_framing_resume: FramingResume::Clean,
            reset_time_unix_nano: 4_000,
            reason_code: TRUNCATE_RESET_REASON_READ_NEW,
        })
        .expect("truncate reset succeeds");
    assert!(outcome.synced);
    let reset = store.table().get(&file_id(1)).expect("record is tracked");
    assert_eq!(reset.file_epoch, 2);
    assert_eq!(reset.committed_offset, 0);

    let outcomes = store
        .quarantine_files(vec![quarantine(2)])
        .expect("quarantine succeeds");
    assert!(outcomes[0].synced);
    assert_eq!(store.stats().quarantined_records, 1);

    let outcome = store
        .reset_quarantined_file(ResetQuarantinedFile {
            file_id: file_id(2),
            expected_quarantine_epoch: 1,
            action: ResetQuarantineAction::ResetToBeginning,
            resulting_epoch: 2,
            resulting_offset: 0,
            new_framing_resume: FramingResume::Clean,
            reset_time_unix_nano: 5_000,
            audit_reason: "operator released quarantine".to_owned(),
        })
        .expect("quarantine reset succeeds");
    assert!(outcome.synced);
    assert_eq!(store.stats().unsynced_transactions, 0);
    assert_eq!(store.stats().quarantined_records, 0);
    assert_eq!(store.stats().quarantine_reset_beginning, 1);
    drop(store);

    let reopened = open(&path);
    assert_eq!(reopened.stats().quarantined_records, 0);
    let reset = reopened
        .table()
        .get(&file_id(1))
        .expect("truncate reset survives");
    assert_eq!(reset.file_epoch, 2);
    assert_eq!(reset.committed_offset, 0);
}

/// Scenario: each quarantine decision (`reset_to_beginning`, `reset_to_end`,
/// and `keep_failed`) is synced, reopened, compacted, and reopened again.
/// Guarantees: both reset actions durably increment the epoch and select the
/// requested offset, while `keep_failed` preserves the original quarantine
/// and its immutable evidence through WAL replay and snapshot compaction.
#[test]
fn every_quarantine_reset_action_survives_reopen_and_compaction() {
    for (name, action, resulting_epoch, resulting_offset, remains_quarantined) in [
        (
            "beginning",
            ResetQuarantineAction::ResetToBeginning,
            2,
            0,
            false,
        ),
        ("end", ResetQuarantineAction::ResetToEnd, 2, 8_192, false),
        ("keep-failed", ResetQuarantineAction::KeepFailed, 1, 0, true),
    ] {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(name);
        let mut store = open(&path);
        let _registered = store
            .register_files(vec![registration(1)])
            .expect("registers");
        let _quarantined = store
            .quarantine_files(vec![quarantine(1)])
            .expect("quarantines");

        let outcome = store
            .reset_quarantined_file(ResetQuarantinedFile {
                file_id: file_id(1),
                expected_quarantine_epoch: 1,
                action,
                resulting_epoch,
                resulting_offset,
                new_framing_resume: FramingResume::Clean,
                reset_time_unix_nano: 5_000,
                audit_reason: format!("operator selected {name}"),
            })
            .expect("the administrative decision succeeds");
        assert!(outcome.synced);
        drop(store);

        let mut reopened = open(&path);
        let record = reopened
            .table()
            .get(&file_id(1))
            .expect("record survives WAL replay");
        assert_eq!(record.file_epoch, resulting_epoch);
        assert_eq!(record.committed_offset, resulting_offset);
        assert_eq!(
            record.lifecycle_state == LifecycleState::Quarantined,
            remains_quarantined
        );
        assert_eq!(record.quarantine_evidence.is_some(), remains_quarantined);

        reopened.compact().expect("the recovered state compacts");
        drop(reopened);
        let compacted = open(&path);
        let record = compacted
            .table()
            .get(&file_id(1))
            .expect("record survives snapshot recovery");
        assert_eq!(record.file_epoch, resulting_epoch);
        assert_eq!(record.committed_offset, resulting_offset);
        assert_eq!(
            record.lifecycle_state == LifecycleState::Quarantined,
            remains_quarantined
        );
        assert_eq!(record.quarantine_evidence.is_some(), remains_quarantined);
    }
}

/// Scenario: an administrative operation submitted without an audit reason.
/// Guarantees: the store refuses it before anything is written, so an
/// unaudited administrative action can never become durable.
#[test]
fn administrative_operations_require_an_audit_reason() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = open(&path);
    let _registered = store
        .register_files(vec![registration(1)])
        .expect("registers");
    let sequence_before = store.stats().next_sequence;

    let error = store
        .reset_quarantined_file(ResetQuarantinedFile {
            file_id: file_id(1),
            expected_quarantine_epoch: 1,
            action: ResetQuarantineAction::KeepFailed,
            resulting_epoch: 1,
            resulting_offset: 0,
            new_framing_resume: FramingResume::Clean,
            reset_time_unix_nano: 5_000,
            audit_reason: String::new(),
        })
        .expect_err("an empty audit reason is refused");
    assert!(matches!(error, StoreError::AuditReasonRequired { .. }));
    assert_eq!(store.stats().next_sequence, sequence_before);
}

/// Scenario: a WAL whose final transaction was cut short by a crash, here
/// simulated by appending a fragment too short to declare a frame.
/// Guarantees: exactly the structurally incomplete trailing bytes are
/// discarded, every complete transaction before them still replays, the WAL
/// file is truncated back to its last complete transaction, and the next
/// append continues the sequence from there.
#[test]
fn torn_final_wal_transaction_is_discarded_and_truncated() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let _seeded = seeded_namespace(&path);

    let wal_path = path.join(wal_file_name(0));
    let complete_len = fs::metadata(&wal_path).expect("wal metadata").len();
    let mut wal = fs::OpenOptions::new()
        .append(true)
        .open(&wal_path)
        .expect("wal opens");
    wal.write_all(&[0x00, 0x00, 0x01]).expect("torn write");
    wal.sync_all().expect("torn write is durable");
    drop(wal);

    let mut store = open(&path);
    assert_eq!(store.recovery().torn_tail_bytes, 3);
    assert_eq!(store.recovery().transactions_replayed, 2);
    assert_eq!(committed_offset(&store, 1), 4_096);
    assert_eq!(
        fs::metadata(&wal_path).expect("wal metadata").len(),
        complete_len
    );
    assert_eq!(store.stats().wal_bytes, complete_len);

    let outcomes = store
        .commit_progress(vec![progress(1, 4_096, 8_192)])
        .expect("progress succeeds");
    assert_eq!(outcomes[0].sequence, 3);
    drop(store);

    let reopened = open(&path);
    assert_eq!(reopened.recovery().torn_tail_bytes, 0);
    assert_eq!(reopened.recovery().transactions_replayed, 3);
    assert_eq!(committed_offset(&reopened, 1), 8_192);
}

/// Scenario: each ordinary WAL write and sync boundary fails in turn while
/// an immediately durable registration is being appended.
/// Guarantees: every injected failure makes the live handle unusable; a
/// reopen recovers the old state before/no-partial-write boundaries and the
/// complete new state after complete-write boundaries, repairing the one
/// intentionally torn tail without exposing a partial transaction.
#[test]
fn wal_append_faults_recover_only_complete_transactions() {
    for point in FaultPoint::WAL_DURABILITY {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("namespace");
        drop(open(&path));

        let mut store = CheckpointStore::open_with_fault(
            StoreOptions {
                sync_interval: Duration::ZERO,
                ..options(&path)
            },
            point,
        )
        .expect("existing namespace opens without appending");
        let error = store
            .register_files(vec![registration(1)])
            .expect_err("the armed WAL boundary fails");
        assert!(
            matches!(error, StoreError::InjectedFault { point: fired } if fired == point),
            "expected the injected fault at {point}, got {error:?}"
        );
        assert!(matches!(
            store.sync().expect_err("the failed handle is unusable"),
            StoreError::Unusable { .. }
        ));
        assert!(matches!(
            store
                .remove_quarantined_file(file_id(9), 1, 1, "cleanup".to_owned())
                .expect_err("an unusable handle cannot report an absent no-op"),
            StoreError::Unusable { .. }
        ));
        assert!(matches!(
            store
                .remove_expired(&HashSet::new(), 1, Duration::from_secs(1), 1,)
                .expect_err("an unusable handle cannot report empty retention"),
            StoreError::Unusable { .. }
        ));
        assert!(matches!(
            store
                .compact_if_due()
                .expect_err("an unusable handle cannot report compaction state"),
            StoreError::Unusable { .. }
        ));
        drop(store);

        let reopened = open(&path);
        let transaction_was_complete = !matches!(
            point,
            FaultPoint::BeforeWalTransactionWrite
                | FaultPoint::DuringWalTransactionWrite
                | FaultPoint::BeforeWalSync
                | FaultPoint::AfterWalSync
        );
        assert_eq!(
            reopened.table().get(&file_id(1)).is_some(),
            transaction_was_complete,
            "unexpected recovered state after a fault at {point}"
        );
        assert_eq!(
            reopened.recovery().transactions_replayed,
            usize::from(transaction_was_complete)
        );
        assert_eq!(
            reopened.recovery().torn_tail_bytes > 0,
            point == FaultPoint::DuringWalTransactionWrite
        );
    }
}

/// Scenario: opening a WAL with a three-byte torn tail fails immediately
/// before repair and immediately after truncation/sync, one boundary at a
/// time.
/// Guarantees: a pre-repair failure leaves the tail for the next open, an
/// after-repair failure leaves the durable valid prefix, and either retry
/// recovers the same complete transactions and table.
#[test]
fn torn_tail_repair_faults_are_resumable() {
    for point in FaultPoint::TORN_TAIL_REPAIR {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("namespace");
        let seeded = seeded_namespace(&path);
        let wal_path = path.join(wal_file_name(0));
        let valid_len = fs::metadata(&wal_path).expect("WAL metadata").len();
        let mut wal = fs::OpenOptions::new()
            .append(true)
            .open(&wal_path)
            .expect("WAL opens");
        wal.write_all(&[0x00, 0x00, 0x01]).expect("torn write");
        wal.sync_all().expect("torn write is durable");
        drop(wal);

        let error = CheckpointStore::open_with_fault(options(&path), point)
            .expect_err("the armed repair boundary fails");
        assert!(
            matches!(error, StoreError::InjectedFault { point: fired } if fired == point),
            "expected the injected fault at {point}, got {error:?}"
        );
        let expected_len = if point == FaultPoint::BeforeTornTailTruncate {
            valid_len + 3
        } else {
            valid_len
        };
        assert_eq!(
            fs::metadata(&wal_path).expect("WAL metadata").len(),
            expected_len
        );

        let reopened = open(&path);
        assert_eq!(records(&reopened), seeded);
        assert_eq!(
            reopened.recovery().torn_tail_bytes,
            if point == FaultPoint::BeforeTornTailTruncate {
                3
            } else {
                0
            }
        );
        assert_eq!(
            fs::metadata(&wal_path).expect("WAL metadata").len(),
            valid_len
        );
    }
}

/// Scenario: a structurally complete WAL transaction whose bytes were
/// altered after they were written.
/// Guarantees: a complete frame with an invalid checksum is corruption, not
/// a torn tail, so recovery fails closed rather than silently dropping
/// acknowledged progress.
#[test]
fn corrupted_wal_transaction_fails_recovery_closed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let _seeded = seeded_namespace(&path);

    let wal_path = path.join(wal_file_name(0));
    let mut bytes = fs::read(&wal_path).expect("wal reads");
    // Flip a byte inside the first transaction's body, past the 24-byte
    // header and its 4-byte length prefix.
    bytes[30] ^= 0xFF;
    write_bytes(&wal_path, &bytes);

    let error = CheckpointStore::open(options(&path)).expect_err("corruption fails closed");
    match error {
        StoreError::Decode { artifact, .. } => assert_eq!(artifact, "WAL"),
        other => panic!("expected a WAL decode failure, got {other:?}"),
    }
}

/// Scenario: a snapshot file whose bytes were altered after it was written.
/// Guarantees: the snapshot has no torn-tail tolerance at all, so any
/// damage to the recovery base fails closed instead of yielding a partial
/// table.
#[test]
fn corrupted_snapshot_fails_recovery_closed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let _seeded = seeded_namespace(&path);

    let snapshot_path = path.join(snapshot_file_name(0));
    let mut bytes = fs::read(&snapshot_path).expect("snapshot reads");
    bytes[12] ^= 0xFF;
    write_bytes(&snapshot_path, &bytes);

    let error = CheckpointStore::open(options(&path)).expect_err("corruption fails closed");
    match error {
        StoreError::Decode { artifact, .. } => assert_eq!(artifact, "snapshot"),
        other => panic!("expected a snapshot decode failure, got {other:?}"),
    }
}

/// Scenario: several appends in one session, a reopen, and a compaction.
/// Guarantees: transaction sequences start at 1 for a generation, increase
/// by exactly one, continue across a reopen of the same generation, and
/// restart at 1 for the new generation compaction creates.
#[test]
fn wal_sequences_are_strict_within_a_generation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = open(&path);

    let first = store
        .register_files(vec![registration(1)])
        .expect("registers");
    assert_eq!(first[0].sequence, 1);
    let second = store
        .commit_progress(vec![progress(1, 0, 64)])
        .expect("progress succeeds");
    assert_eq!(second[0].sequence, 2);
    drop(store);

    let mut reopened = open(&path);
    assert_eq!(reopened.stats().next_sequence, 3);
    let third = reopened
        .commit_progress(vec![progress(1, 64, 128)])
        .expect("progress succeeds");
    assert_eq!(third[0].sequence, 3);

    reopened.compact().expect("compaction succeeds");
    assert_eq!(reopened.stats().next_sequence, 1);
    let after_compaction = reopened
        .commit_progress(vec![progress(1, 128, 192)])
        .expect("progress succeeds");
    assert_eq!(after_compaction[0].sequence, 1);
}

/// Scenario: compaction of a namespace that already holds durable state,
/// followed by the explicit cleanup step.
/// Guarantees: compaction publishes a complete new generation and repoints
/// `CURRENT` at it while the previous generation stays on disk and
/// recoverable; cleanup then removes only the retired generation's files and
/// leaves the marker, the live pair, and the ownership lock intact.
#[test]
fn compaction_keeps_the_previous_generation_until_cleanup() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let seeded = seeded_namespace(&path);

    let mut store = open(&path);
    store.compact().expect("compaction succeeds");
    assert_eq!(store.generation(), 1);
    assert_eq!(store.retired_generations(), [0]);
    assert_eq!(records(&store), seeded);
    assert_eq!(store.stats().wal_transactions, 0);
    assert!(path.join(snapshot_file_name(0)).is_file());
    assert!(path.join(wal_file_name(0)).is_file());
    assert!(path.join(snapshot_file_name(1)).is_file());
    assert!(path.join(wal_file_name(1)).is_file());
    drop(store);

    // The previous generation is still on disk, but the marker alone decides
    // which one is authoritative.
    let mut reopened = open(&path);
    assert_eq!(reopened.generation(), 1);
    assert_eq!(reopened.recovery().retired_generations, vec![0]);
    assert_eq!(records(&reopened), seeded);

    assert_eq!(
        reopened
            .cleanup_retired_generations()
            .expect("cleanup succeeds"),
        1
    );
    assert!(!path.join(snapshot_file_name(0)).exists());
    assert!(!path.join(wal_file_name(0)).exists());
    assert!(path.join(snapshot_file_name(1)).is_file());
    assert!(path.join(wal_file_name(1)).is_file());
    assert!(path.join(CURRENT_FILE_NAME).is_file());
    assert!(path.join(OWNERSHIP_LOCK_FILE_NAME).is_file());
    assert!(reopened.retired_generations().is_empty());
}

/// Scenario: compaction interrupted at each persistence boundary in turn --
/// every snapshot, WAL, marker, and directory-sync step.
/// Guarantees: whatever step fails, reopening selects one complete
/// generation and recovers exactly the pre-compaction table: either the old
/// generation (before the marker is replaced) or the new one (after), never
/// a mixture of the two, and never a partially written file.
#[test]
fn compaction_recovers_a_single_complete_generation_after_any_fault() {
    for point in FaultPoint::PUBLICATION {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("namespace");
        let seeded = seeded_namespace(&path);

        let mut store = CheckpointStore::open_with_fault(options(&path), point)
            .expect("opening an existing namespace publishes nothing");
        let error = store.compact().expect_err("the armed boundary fails");
        assert!(
            matches!(error, StoreError::InjectedFault { point: fired } if fired == point),
            "expected the injected fault at {point}, got {error:?}"
        );
        let marker_already_replaced = matches!(
            point,
            FaultPoint::AfterMarkerPublish
                | FaultPoint::BeforeMarkerDirSync
                | FaultPoint::AfterMarkerDirSync
        );
        if marker_already_replaced {
            // The authoritative generation changed under the store, so it
            // refuses further work rather than appending to a WAL that is no
            // longer the selected generation's.
            let refused = store
                .commit_progress(vec![progress(2, 0, 32)])
                .expect_err("an unusable store refuses further appends");
            assert!(matches!(refused, StoreError::Unusable { .. }));
        }
        drop(store);

        let reopened = open(&path);
        let expected_generation = u64::from(marker_already_replaced);
        assert_eq!(
            reopened.generation(),
            expected_generation,
            "unexpected generation after a fault at {point}"
        );
        assert_eq!(
            records(&reopened),
            seeded,
            "recovered a mixed table after a fault at {point}"
        );
        assert!(path.join(snapshot_file_name(expected_generation)).is_file());
        assert!(path.join(wal_file_name(expected_generation)).is_file());
        assert_eq!(
            reopened.recovery().removed_temp_files > 0,
            matches!(
                point,
                FaultPoint::AfterSnapshotWrite
                    | FaultPoint::AfterSnapshotSync
                    | FaultPoint::AfterWalWrite
                    | FaultPoint::AfterGenerationWalSync
                    | FaultPoint::AfterMarkerWrite
                    | FaultPoint::AfterMarkerSync
            ),
            "unexpected temporary-file cleanup after a fault at {point}"
        );
    }
}

/// Scenario: creation of a namespace's first generation interrupted at each
/// persistence boundary in turn.
/// Guarantees: no partially created namespace is ever adopted -- the failed
/// open reports the boundary it failed at, and a later open always yields a
/// complete, empty, marker-selected generation 0 with no abandoned
/// temporary files left behind.
#[test]
fn initial_creation_recovers_a_complete_generation_after_any_fault() {
    for point in FaultPoint::PUBLICATION {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("namespace");

        let error = CheckpointStore::open_with_fault(options(&path), point)
            .expect_err("the armed boundary fails");
        assert!(
            matches!(error, StoreError::InjectedFault { point: fired } if fired == point),
            "expected the injected fault at {point}, got {error:?}"
        );

        let store = open(&path);
        assert_eq!(
            store.generation(),
            0,
            "unexpected generation after a fault at {point}"
        );
        assert_eq!(store.table().len(), 0);
        assert!(path.join(CURRENT_FILE_NAME).is_file());
        assert!(path.join(snapshot_file_name(0)).is_file());
        assert!(path.join(wal_file_name(0)).is_file());
        drop(store);

        let leftover_temp_files = fs::read_dir(&path)
            .expect("namespace lists")
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.ends_with(".tmp"))
            })
            .count();
        assert_eq!(
            leftover_temp_files, 0,
            "temporary files survived a fault at {point}"
        );
    }
}

/// Scenario: a namespace whose `CURRENT` marker was lost while it still held
/// only its first generation, one whose marker was lost after it had been
/// compacted, and one whose marker was lost after a compaction that left the
/// newer generation incomplete while the older pair was still on disk.
/// Guarantees: the highest complete snapshot/WAL pair is adopted only for a
/// first-store namespace, where an interrupted creation is the sole way the
/// marker can be absent; once a namespace has advanced past its first
/// generation, a missing marker fails closed rather than guessing which
/// generation was authoritative -- in particular it never falls back to a
/// stale, still-present generation 0 just because the newer generation is
/// incomplete, which would silently revert every transaction recorded since
/// compaction.
#[test]
fn missing_marker_adopts_a_pair_only_for_a_first_store_namespace() {
    let dir = tempfile::tempdir().expect("temp dir");
    let first_store = dir.path().join("first-store");
    let seeded = seeded_namespace(&first_store);
    fs::remove_file(first_store.join(CURRENT_FILE_NAME)).expect("removes the marker");

    let adopted = open(&first_store);
    assert!(adopted.recovery().adopted_without_marker);
    assert!(!adopted.recovery().created);
    assert_eq!(adopted.generation(), 0);
    assert_eq!(records(&adopted), seeded);
    drop(adopted);

    let compacted = dir.path().join("compacted");
    let _seeded = seeded_namespace(&compacted);
    let mut store = open(&compacted);
    store.compact().expect("compaction succeeds");
    drop(store);
    fs::remove_file(compacted.join(CURRENT_FILE_NAME)).expect("removes the marker");

    let error = CheckpointStore::open(options(&compacted)).expect_err("fails closed");
    match error {
        StoreError::MissingMarker {
            highest_generation, ..
        } => assert_eq!(highest_generation, 1),
        other => panic!("expected a missing-marker failure, got {other:?}"),
    }

    // The newer generation is incomplete, so the only complete pair left is
    // the retained generation 0. Adopting it would discard everything
    // recorded since compaction.
    for missing_half in [wal_file_name(1), snapshot_file_name(1)] {
        let stale = dir.path().join(format!("stale-{missing_half}"));
        let _seeded = seeded_namespace(&stale);
        let mut store = open(&stale);
        store.compact().expect("compaction succeeds");
        let _progressed = store
            .commit_progress(vec![progress(1, 4_096, 8_192)])
            .expect("progress succeeds");
        store.drain().expect("drain syncs");
        drop(store);
        fs::remove_file(stale.join(CURRENT_FILE_NAME)).expect("removes the marker");
        fs::remove_file(stale.join(&missing_half)).expect("removes half of generation 1");

        let error = CheckpointStore::open(options(&stale)).expect_err("fails closed");
        match error {
            StoreError::MissingMarker {
                highest_generation, ..
            } => assert_eq!(highest_generation, 1),
            other => panic!("expected a missing-marker failure, got {other:?}"),
        }
    }
}

/// Scenario: a marker-less namespace whose initial generation is missing one
/// half of its pair, once where the surviving half is empty and once where
/// it already holds durable transactions.
/// Guarantees: an interrupted creation (both halves empty) is recreated,
/// while a pair whose surviving half carries state fails closed instead of
/// being silently replaced by an empty generation.
#[test]
fn missing_marker_with_an_incomplete_initial_pair_only_recreates_empty_state() {
    let dir = tempfile::tempdir().expect("temp dir");

    let interrupted = dir.path().join("interrupted");
    drop(open(&interrupted));
    fs::remove_file(interrupted.join(CURRENT_FILE_NAME)).expect("removes the marker");
    fs::remove_file(interrupted.join(wal_file_name(0))).expect("removes the WAL");

    let recreated = open(&interrupted);
    assert!(recreated.recovery().created);
    assert_eq!(recreated.generation(), 0);
    assert_eq!(recreated.table().len(), 0);
    assert!(interrupted.join(CURRENT_FILE_NAME).is_file());
    assert!(interrupted.join(wal_file_name(0)).is_file());
    drop(recreated);

    let populated = dir.path().join("populated");
    let _seeded = seeded_namespace(&populated);
    fs::remove_file(populated.join(CURRENT_FILE_NAME)).expect("removes the marker");
    fs::remove_file(populated.join(snapshot_file_name(0))).expect("removes the snapshot");

    let error = CheckpointStore::open(options(&populated)).expect_err("fails closed");
    match error {
        StoreError::IncompleteInitialGeneration { reason, .. } => {
            assert_eq!(reason, "its WAL already holds transactions");
        }
        other => panic!("expected an incomplete initial generation, got {other:?}"),
    }
}

/// Scenario: a marker-less first-store namespace whose complete pair is
/// corrupt.
/// Guarantees: adopting a pair without a marker still validates it in full,
/// so a damaged pair fails closed instead of being adopted as authoritative.
#[test]
fn missing_marker_still_validates_the_adopted_pair() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let _seeded = seeded_namespace(&path);
    fs::remove_file(path.join(CURRENT_FILE_NAME)).expect("removes the marker");

    let snapshot_path = path.join(snapshot_file_name(0));
    let mut bytes = fs::read(&snapshot_path).expect("snapshot reads");
    bytes[10] ^= 0xFF;
    write_bytes(&snapshot_path, &bytes);

    let error = CheckpointStore::open(options(&path)).expect_err("fails closed");
    assert!(matches!(error, StoreError::Decode { .. }));
}

/// Scenario: a retention pass over a namespace holding an idle active
/// record, an idle rotated-finalized record, and an idle quarantined record.
/// Guarantees: ordinary retention removes only the non-quarantined records;
/// a quarantined record is exempt however old it is, and disappears only
/// through an administrative removal that names this namespace and carries
/// an audit reason.
#[test]
fn retention_never_removes_a_quarantined_record() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = open(&path);
    let _registered = store
        .register_files(vec![registration(1), registration(2), registration(3)])
        .expect("registers");
    let _quarantined = store
        .quarantine_files(vec![quarantine(2)])
        .expect("quarantine succeeds");
    let mut finalize = progress(3, 0, 2_048);
    finalize.finalize = true;
    let _finalized = store
        .commit_progress(vec![finalize])
        .expect("finalizing progress succeeds");
    assert_eq!(
        store
            .table()
            .get(&file_id(3))
            .expect("record is tracked")
            .lifecycle_state,
        LifecycleState::RotatedFinalized
    );

    let now = 10_000_000_000u64;
    let retention = Duration::from_secs(1);
    let eligible_absent = HashSet::from([file_id(1), file_id(2), file_id(3)]);
    let candidates = store.retention_candidates(&eligible_absent, now, retention);
    assert_eq!(candidates, vec![file_id(1), file_id(3)]);

    let removed = store
        .remove_expired(&eligible_absent, now, retention, 0x0007)
        .expect("retention removal succeeds");
    assert_eq!(removed, 2);
    assert!(store.table().get(&file_id(1)).is_none());
    assert!(store.table().get(&file_id(3)).is_none());
    let quarantined = store.table().get(&file_id(2)).expect("quarantine survives");
    assert_eq!(quarantined.lifecycle_state, LifecycleState::Quarantined);

    // Zero retention means indefinite retention and selects nothing.
    assert!(
        store
            .retention_candidates(&eligible_absent, now, Duration::ZERO)
            .is_empty()
    );

    let outcome = store
        .remove_quarantined_file(file_id(2), 0x0008, now, "operator purge".to_owned())
        .expect("administrative removal succeeds")
        .expect("the record was present");
    assert!(outcome.synced);
    assert!(store.table().get(&file_id(2)).is_none());
    drop(store);

    let reopened = open(&path);
    assert_eq!(reopened.table().len(), 0);
}

/// Scenario: pipeline drain after a progress transaction whose sync the
/// configured interval deferred.
/// Guarantees: drain forces the outstanding sync, so a clean shutdown never
/// leaves acknowledged progress unsynced, and a second drain with nothing
/// outstanding performs no further sync.
#[test]
fn drain_syncs_outstanding_progress() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = CheckpointStore::open(StoreOptions {
        sync_interval: NEVER_ELAPSES,
        ..options(&path)
    })
    .expect("namespace opens");
    let _registered = store
        .register_files(vec![registration(1)])
        .expect("registers");
    let _progressed = store
        .commit_progress(vec![progress(1, 0, 512)])
        .expect("progress succeeds");
    assert_eq!(store.stats().unsynced_transactions, 1);

    let syncs_before = store.stats().syncs;
    store.drain().expect("drain syncs");
    assert_eq!(store.stats().unsynced_transactions, 0);
    assert_eq!(store.stats().syncs, syncs_before + 1);

    store.drain().expect("a second drain is a no-op");
    assert_eq!(store.stats().syncs, syncs_before + 1);
    // WAL accounting tracks the file exactly, so compaction thresholds are
    // measured against real bytes rather than an estimate.
    assert_eq!(
        store.stats().wal_bytes,
        fs::metadata(path.join(wal_file_name(0)))
            .expect("wal metadata")
            .len()
    );
    assert_eq!(store.stats().wal_transactions, 2);
    drop(store);

    let reopened = open(&path);
    assert_eq!(committed_offset(&reopened, 1), 512);
}

/// Scenario: opening a namespace with an id the durable format cannot
/// represent.
/// Guarantees: the store refuses at open rather than at the first
/// administrative operation, so a namespace whose administrative removals
/// could never name it correctly is never taken ownership of.
#[test]
fn open_rejects_a_namespace_id_the_format_cannot_represent() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");

    let empty = StoreOptions {
        namespace_id: String::new(),
        ..options(&path)
    };
    assert!(matches!(
        CheckpointStore::open(empty).expect_err("an empty id is refused"),
        StoreError::InvalidNamespaceId { .. }
    ));
    assert!(!path.exists());

    let oversized = StoreOptions {
        namespace_id: "n".repeat(NAMESPACE_ID_MAX_BYTES + 1),
        ..options(&path)
    };
    assert!(matches!(
        CheckpointStore::open(oversized).expect_err("an oversized id is refused"),
        StoreError::InvalidNamespaceId { .. }
    ));
    assert!(!path.exists());

    // The namespace is still openable with a valid id.
    let store = open(&path);
    assert_eq!(store.namespace_id(), NAMESPACE_ID);
}

/// Scenario: inspecting the namespace directory and every file the store
/// creates in it on a Unix host.
/// Guarantees: durable checkpoint state, which records the paths and read
/// offsets of collected log files, is never group- or world-accessible.
#[cfg(unix)]
#[test]
fn namespace_files_are_not_group_or_world_accessible() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = open(&path);
    let _registered = store
        .register_files(vec![registration(1)])
        .expect("registers");
    store.compact().expect("compaction succeeds");
    drop(store);

    let accessible_beyond_owner = |target: PathBuf| {
        fs::metadata(&target)
            .unwrap_or_else(|error| panic!("{} is readable: {error}", target.display()))
            .permissions()
            .mode()
            & 0o077
    };
    assert_eq!(accessible_beyond_owner(path.clone()), 0);
    assert_eq!(accessible_beyond_owner(path.join(CURRENT_FILE_NAME)), 0);
    assert_eq!(accessible_beyond_owner(path.join(snapshot_file_name(1))), 0);
    assert_eq!(accessible_beyond_owner(path.join(wal_file_name(1))), 0);
    assert_eq!(
        accessible_beyond_owner(path.join(OWNERSHIP_LOCK_FILE_NAME)),
        0
    );
}

/// Scenario: opening a namespace that contains abandoned temporary files
/// from an interrupted write alongside unrelated files.
/// Guarantees: recovery removes exactly the temporary names this store
/// writes and nothing else -- unrelated `.tmp` files, the marker, the live
/// generation pair, and the ownership lock all survive.
#[test]
fn temporary_file_cleanup_removes_only_this_namespace_temporaries() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    drop(open(&path));

    let mut owned = vec![temp_file_name(CURRENT_FILE_NAME)];
    for generation in 1..=MAX_GENERATIONS_ON_DISK as u64 {
        owned.push(temp_file_name(&snapshot_file_name(generation)));
        owned.push(temp_file_name(&wal_file_name(generation)));
    }
    assert_eq!(owned.len(), MAX_TEMP_FILES);
    let foreign = [
        "unrelated.tmp".to_owned(),
        "offsets-x.snapshot.tmp".to_owned(),
        "offsets-03.wal.tmp".to_owned(),
        "CURRENT.tmp.keep".to_owned(),
        "notes.txt".to_owned(),
    ];
    for name in owned.iter().chain(foreign.iter()) {
        write_bytes(&path.join(name), b"scratch");
    }

    let store = open(&path);
    assert_eq!(store.recovery().removed_temp_files, owned.len());
    for name in &owned {
        assert!(!path.join(name).exists(), "{name} should have been removed");
    }
    for name in &foreign {
        assert!(path.join(name).is_file(), "{name} should have survived");
    }
    assert!(path.join(CURRENT_FILE_NAME).is_file());
    assert!(path.join(snapshot_file_name(0)).is_file());
    assert!(path.join(wal_file_name(0)).is_file());
    assert!(path.join(OWNERSHIP_LOCK_FILE_NAME).is_file());
}

/// Scenario: a namespace contains one more recognized abandoned temporary
/// artifact than bounded recovery cleanup permits.
/// Guarantees: opening fails before deleting any candidate, so an
/// adversarial directory cannot turn one open into unbounded cleanup work
/// or make repeated opens erase an unbounded population in chunks.
#[test]
fn excessive_temporary_file_population_is_rejected_without_cleanup() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    drop(open(&path));

    let mut names = vec![temp_file_name(CURRENT_FILE_NAME)];
    for generation in 0..MAX_GENERATIONS_ON_DISK as u64 + 1 {
        names.push(temp_file_name(&snapshot_file_name(generation)));
        names.push(temp_file_name(&wal_file_name(generation)));
    }
    names.truncate(MAX_TEMP_FILES + 1);
    for name in &names {
        write_bytes(&path.join(name), b"stale");
    }

    let error =
        CheckpointStore::open(options(&path)).expect_err("the excessive population is refused");
    assert!(matches!(
        error,
        StoreError::TooManyTemporaryFiles { max, .. } if max == MAX_TEMP_FILES
    ));
    assert!(
        names.iter().all(|name| path.join(name).is_file()),
        "the refusal must precede cleanup"
    );
}

/// Scenario: a namespace has exactly the maximum recognized generation
/// population and then one generation beyond it.
/// Guarantees: the compatibility allowance opens and can be cleaned, while
/// the first excess generation fails with an explicit population error and
/// unrelated directory entries remain ignored.
#[test]
fn excessive_generation_population_is_rejected() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    drop(open(&path));
    for generation in 1..MAX_GENERATIONS_ON_DISK as u64 {
        write_bytes(&path.join(snapshot_file_name(generation)), b"not selected");
    }
    write_bytes(&path.join("unrelated.snapshot"), b"ignored");

    let mut accepted = open(&path);
    assert_eq!(accepted.retired_generations(), [1, 2]);
    assert_eq!(
        accepted
            .cleanup_retired_generations()
            .expect("the bounded population is cleaned"),
        2
    );
    drop(accepted);

    for generation in 1..=MAX_GENERATIONS_ON_DISK as u64 {
        write_bytes(&path.join(snapshot_file_name(generation)), b"not selected");
    }
    let error =
        CheckpointStore::open(options(&path)).expect_err("the excessive population is refused");
    assert!(matches!(
        error,
        StoreError::TooManyGenerations { max, .. } if max == MAX_GENERATIONS_ON_DISK
    ));
}

/// Scenario: a second store opened on a namespace another store already
/// owns, and then reopened after the first store is dropped.
/// Guarantees: namespace ownership is exclusive even within one process, the
/// wait for it is bounded by the configured ownership timeout rather than
/// blocking forever, and the lock is released when the owning store is
/// dropped.
#[test]
fn namespace_ownership_is_exclusive_and_bounded() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let owner = open(&path);

    let timeout = Duration::from_millis(120);
    let contended = StoreOptions {
        ownership_timeout: timeout,
        ownership_retry_interval: Duration::from_millis(10),
        ..options(&path)
    };
    let started = Instant::now();
    let error = CheckpointStore::open(contended).expect_err("the namespace is owned");
    let waited = started.elapsed();
    match error {
        StoreError::NamespaceLocked {
            waited: reported,
            timeout: reported_timeout,
            ..
        } => {
            assert_eq!(reported_timeout, timeout);
            assert!(reported <= waited);
        }
        other => panic!("expected a namespace lock failure, got {other:?}"),
    }
    assert!(waited >= timeout, "acquisition returned before the timeout");
    assert!(
        waited < timeout * 10,
        "acquisition waited far past the timeout"
    );

    drop(owner);
    let successor = open(&path);
    assert_eq!(successor.generation(), 0);
}

/// Scenario: transactions that are empty, larger than the format allows, or
/// carry an operation that cannot replay against current durable state.
/// Guarantees: each is refused before any byte reaches the WAL, so the
/// durable log can never contain a transaction that a later recovery would
/// reject.
#[test]
fn invalid_transactions_are_refused_before_they_are_written() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = open(&path);
    let _registered = store
        .register_files(vec![registration(1)])
        .expect("registers");
    let bytes_before = store.stats().wal_bytes;
    let sequence_before = store.stats().next_sequence;

    assert!(matches!(
        store.append(Vec::new()).expect_err("empty is refused"),
        StoreError::EmptyTransaction
    ));

    let oversized: Vec<Operation> = (0..=usize::from(WAL_MAX_OPS_PER_TX))
        .map(|index| {
            let mut bytes = [0u8; 16];
            bytes[0..8].copy_from_slice(&(index as u64).to_be_bytes());
            let mut register = registration(1);
            register.file_id = FileId(bytes);
            Operation::RegisterFile(register)
        })
        .collect();
    assert!(matches!(
        store.append(oversized).expect_err("oversized is refused"),
        StoreError::TransactionTooLarge { .. }
    ));

    // The expected offset does not match durable state, so replay would
    // reject this operation.
    let stale = store
        .commit_progress(vec![progress(1, 999, 1_024)])
        .expect_err("a stale precondition is refused");
    assert!(matches!(stale, StoreError::Apply { .. }));

    assert_eq!(store.stats().wal_bytes, bytes_before);
    assert_eq!(store.stats().next_sequence, sequence_before);
    assert_eq!(committed_offset(&store, 1), 0);
    drop(store);

    let reopened = open(&path);
    assert_eq!(reopened.table().len(), 1);
    assert_eq!(committed_offset(&reopened, 1), 0);
}

/// Scenario: a namespace configured to compact after two transactions.
/// Guarantees: the store reports the threshold as met through the append
/// outcome and `compaction_due`, and `compact_if_due` then publishes a new
/// generation whose WAL starts empty while the recovered table is unchanged.
#[test]
fn compaction_threshold_triggers_a_new_generation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = CheckpointStore::open(StoreOptions {
        compact_after_transactions: 2,
        ..options(&path)
    })
    .expect("namespace opens");

    let registered = store
        .register_files(vec![registration(1)])
        .expect("registers");
    assert!(!registered[0].compaction_due);
    let progressed = store
        .commit_progress(vec![progress(1, 0, 256)])
        .expect("progress succeeds");
    assert!(progressed[0].compaction_due);
    assert!(store.compaction_due());

    assert!(store.compact_if_due().expect("compaction succeeds"));
    assert_eq!(store.generation(), 1);
    assert_eq!(store.stats().wal_transactions, 0);
    assert!(!store.compaction_due());
    assert!(!store.compact_if_due().expect("no compaction is due"));
    assert_eq!(committed_offset(&store, 1), 256);
    drop(store);

    let reopened = open(&path);
    assert_eq!(reopened.generation(), 1);
    assert_eq!(reopened.recovery().snapshot_records, 1);
    assert_eq!(reopened.recovery().transactions_replayed, 0);
    assert_eq!(committed_offset(&reopened, 1), 256);
}

/// Scenario: the size bounds a store resolves from its options, for the
/// shipped defaults and for options whose worst case cannot be honored.
/// Guarantees: read caps are derived from the same knobs that govern
/// writing rather than being independent settings, the default
/// configuration resolves well inside the artifact ceiling, and options
/// that could only produce an unreadable namespace are refused at `open`
/// before the directory is created or locked.
#[test]
fn store_limits_are_derived_from_the_configured_bounds() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");

    let defaults = options(&path);
    let expected = StoreLimits::derive(
        defaults.compact_after_bytes,
        defaults.max_tracked_files,
        defaults.fingerprint_bytes,
    )
    .expect("the default configuration is representable");
    let store = open(&path);
    assert_eq!(store.limits(), expected);
    assert!(expected.max_snapshot_bytes <= ARTIFACT_BYTES_CEILING);
    assert!(expected.max_wal_bytes <= ARTIFACT_BYTES_CEILING);
    // The WAL cap must leave room for one maximal transaction on top of the
    // compaction threshold, or a caller that compacts exactly when due
    // could still be unable to write.
    assert!(
        expected.max_wal_bytes >= defaults.compact_after_bytes + expected.max_transaction_bytes
    );
    drop(store);

    let unbounded = dir.path().join("unbounded");
    let refused = CheckpointStore::open(StoreOptions {
        max_tracked_files: u32::MAX,
        ..options(&unbounded)
    })
    .expect_err("a snapshot bound past the ceiling is refused");
    assert!(matches!(refused, StoreError::ResourceBounds { .. }));
    assert!(!unbounded.exists(), "the namespace must not be created");

    let unbounded_wal = dir.path().join("unbounded-wal");
    let refused = CheckpointStore::open(StoreOptions {
        compact_after_bytes: ARTIFACT_BYTES_CEILING,
        ..options(&unbounded_wal)
    })
    .expect_err("a WAL bound past the ceiling is refused");
    assert!(matches!(refused, StoreError::ResourceBounds { .. }));
    assert!(!unbounded_wal.exists(), "the namespace must not be created");
}

/// Scenario: progress transactions appended one at a time to a namespace
/// with a small compaction threshold, up to and past the transaction that
/// crosses it.
/// Guarantees: exactly one transaction takes the WAL from under the
/// threshold to at or over it, the WAL never leaves the cap recovery reads
/// it back with, and compacting at that point starts an empty WAL whose
/// state still reopens intact.
#[test]
fn crossing_the_compaction_threshold_keeps_the_wal_within_its_recovery_cap() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = CheckpointStore::open(StoreOptions {
        compact_after_bytes: 4_096,
        compact_after_transactions: u32::MAX,
        sync_interval: NEVER_ELAPSES,
        ..options(&path)
    })
    .expect("namespace opens");
    let limits = store.limits();
    let _registered = store
        .register_files(vec![registration(1)])
        .expect("registers");

    let mut offset = 0u64;
    let mut crossings = 0usize;
    while !store.compaction_due() {
        let before = store.stats().wal_bytes;
        assert!(before < 4_096, "the loop must stop at the threshold");
        let outcomes = store
            .commit_progress(vec![progress(1, offset, offset + 64)])
            .expect("progress succeeds");
        offset += 64;
        let after = store.stats().wal_bytes;
        if after >= 4_096 {
            crossings += 1;
            assert!(outcomes[0].compaction_due);
        }
        assert!(
            after <= limits.max_wal_bytes,
            "the WAL grew past the cap recovery reads it back with"
        );
    }
    assert_eq!(crossings, 1, "the threshold must be crossed exactly once");

    assert!(store.compact_if_due().expect("compaction succeeds"));
    assert_eq!(store.generation(), 1);
    assert_eq!(store.stats().wal_transactions, 0);
    assert_eq!(committed_offset(&store, 1), offset);
    drop(store);

    let reopened = open(&path);
    assert_eq!(reopened.generation(), 1);
    assert_eq!(committed_offset(&reopened, 1), offset);
}

/// Scenario: a store whose compaction threshold is the smallest legal one,
/// asked to append a maximal transaction to a WAL that has been filled to
/// the point where one no longer fits, then again after compaction.
/// Guarantees: an append that would push the WAL past the largest WAL this
/// configuration can recover is refused with the in-memory table and the
/// WAL untouched, compaction frees the whole budget so the identical
/// transaction then succeeds, and a WAL filled to that legal maximum still
/// reopens and replays completely.
#[test]
fn an_append_past_the_wal_cap_is_refused_and_succeeds_after_compaction() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    // The smallest legal threshold: compaction is due immediately, so the
    // only WAL budget a maximal transaction ever has is the one a freshly
    // compacted generation gives it.
    let bounded = || StoreOptions {
        compact_after_bytes: 1,
        fingerprint_bytes: 16,
        max_tracked_files: u32::from(WAL_MAX_OPS_PER_TX) + 128,
        ..options(&path)
    };
    let mut store = CheckpointStore::open(bounded()).expect("namespace opens");
    let limits = store.limits();

    let maximal: Vec<RegisterFile> = (0..u64::from(WAL_MAX_OPS_PER_TX))
        .map(widest_registration)
        .collect();
    let maximal_bytes = Transaction {
        sequence: 1,
        operations: maximal
            .clone()
            .into_iter()
            .map(Operation::RegisterFile)
            .collect(),
    }
    .encode()
    .expect("the maximal transaction encodes")
    .len() as u64;
    assert!(maximal_bytes <= limits.max_transaction_bytes);

    // Fill the WAL until one maximal transaction no longer fits within the
    // cap, using the store's own accounting rather than a hard-coded size.
    let mut filler = u64::from(WAL_MAX_OPS_PER_TX);
    while store.stats().wal_bytes + maximal_bytes <= limits.max_wal_bytes {
        let _registered = store
            .register_files(vec![widest_registration(filler)])
            .expect("registers");
        filler += 1;
    }
    let filled = store.table().len();
    let bytes_before = store.stats().wal_bytes;
    let sequence_before = store.stats().next_sequence;

    let refused = store
        .append(
            maximal
                .clone()
                .into_iter()
                .map(Operation::RegisterFile)
                .collect(),
        )
        .expect_err("the WAL has no room for a maximal transaction");
    match refused {
        StoreError::WalWouldExceedMaximum {
            wal_bytes,
            transaction_bytes,
            max,
            ..
        } => {
            assert_eq!(wal_bytes, bytes_before);
            assert_eq!(transaction_bytes, maximal_bytes);
            assert_eq!(max, limits.max_wal_bytes);
        }
        other => panic!("expected a WAL capacity refusal, got {other:?}"),
    }
    // Nothing advanced: the same transaction can be retried unchanged.
    assert_eq!(store.stats().wal_bytes, bytes_before);
    assert_eq!(store.stats().next_sequence, sequence_before);
    assert_eq!(store.table().len(), filled);

    store.compact().expect("compaction succeeds");
    let outcomes = store.register_files(maximal).expect("registers");
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].operations, usize::from(WAL_MAX_OPS_PER_TX));
    let wal_bytes = store.stats().wal_bytes;
    assert!(wal_bytes <= limits.max_wal_bytes);
    assert_eq!(
        wal_bytes,
        fs::metadata(path.join(wal_file_name(1)))
            .expect("wal metadata")
            .len()
    );
    let tracked = store.table().len();
    assert_eq!(tracked, filled + usize::from(WAL_MAX_OPS_PER_TX));
    drop(store);

    let reopened =
        CheckpointStore::open(bounded()).expect("a WAL filled to its legal maximum reopens");
    assert_eq!(reopened.table().len(), tracked);
    assert_eq!(reopened.recovery().transactions_replayed, 1);
    assert_eq!(reopened.recovery().torn_tail_bytes, 0);
}

/// Scenario: a namespace whose WAL, and one whose snapshot, is larger than
/// the cap the same configuration derives.
/// Guarantees: an oversized artifact is refused by its length before any
/// buffer is allocated for it, and the failure names the artifact and the
/// cap instead of silently recovering a truncated prefix.
#[test]
fn artifacts_larger_than_the_derived_caps_are_refused_before_buffering() {
    let dir = tempfile::tempdir().expect("temp dir");
    let limits = options(&dir.path().join("probe"))
        .limits()
        .expect("the default configuration is representable");

    let wal_path = dir.path().join("oversized-wal");
    let _seeded = seeded_namespace(&wal_path);
    grow_file(&wal_path.join(wal_file_name(0)), limits.max_wal_bytes + 1);
    match CheckpointStore::open(options(&wal_path)).expect_err("an oversized WAL is refused") {
        StoreError::FileTooLarge {
            artifact, len, max, ..
        } => {
            assert_eq!(artifact, "WAL");
            assert_eq!(len, limits.max_wal_bytes + 1);
            assert_eq!(max, limits.max_wal_bytes);
        }
        other => panic!("expected an oversized WAL refusal, got {other:?}"),
    }

    let snapshot_path = dir.path().join("oversized-snapshot");
    let _seeded = seeded_namespace(&snapshot_path);
    grow_file(
        &snapshot_path.join(snapshot_file_name(0)),
        limits.max_snapshot_bytes + 1,
    );
    match CheckpointStore::open(options(&snapshot_path))
        .expect_err("an oversized snapshot is refused")
    {
        StoreError::FileTooLarge {
            artifact, len, max, ..
        } => {
            assert_eq!(artifact, "snapshot");
            assert_eq!(len, limits.max_snapshot_bytes + 1);
            assert_eq!(max, limits.max_snapshot_bytes);
        }
        other => panic!("expected an oversized snapshot refusal, got {other:?}"),
    }
}

/// Scenario: a WAL tail declares one transaction frame larger than the
/// configured per-transaction bound while the WAL file itself remains far
/// below its whole-artifact cap.
/// Guarantees: recovery rejects the declared frame size before decoding or
/// allocating its operations, preserving the peak-memory formula's
/// one-bounded-transaction assumption.
#[test]
fn oversized_wal_transaction_is_rejected_before_decode() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    drop(open(&path));
    let limits = options(&path)
        .limits()
        .expect("the default bounds are representable");
    let declared_body = u32::try_from(limits.max_transaction_bytes - 7)
        .expect("the default transaction bound fits u32");
    let mut bytes = encode_wal(0, &[]).expect("the WAL header encodes");
    bytes.extend_from_slice(&declared_body.to_be_bytes());
    write_bytes(&path.join(wal_file_name(0)), &bytes);

    let error =
        CheckpointStore::open(options(&path)).expect_err("the oversized transaction is refused");
    assert!(matches!(
        error,
        StoreError::FileTooLarge {
            artifact: "WAL transaction",
            len,
            max,
            ..
        } if len == limits.max_transaction_bytes + 1 && max == limits.max_transaction_bytes
    ));
}

/// Scenario: a checkpoint artifact grows past its read cap after its
/// preallocation size has already been selected.
/// Guarantees: the bounded reader consumes at most one byte beyond the cap
/// and reports `FileTooLarge` instead of returning a valid-looking capped
/// prefix to the decoder.
#[test]
fn artifact_growth_during_a_bounded_read_is_rejected() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("artifact");
    write_bytes(&path, b"12345");
    let file = fs::File::open(&path).expect("artifact opens");
    let error = super::fsio::read_bounded_contents(file, 4, &path, "test artifact", 4)
        .expect_err("growth beyond the selected capacity is rejected");
    assert!(matches!(
        error,
        StoreError::FileTooLarge {
            artifact: "test artifact",
            len: 5,
            max: 4,
            ..
        }
    ));
}

/// Scenario: a test-only snapshot cap is tightened below the current
/// table's encoded size immediately before compaction.
/// Guarantees: compaction independently refuses to publish a snapshot the
/// store could not read back, before writing any byte, even though the
/// production tracked-file admission guard normally makes this unreachable.
#[test]
fn compaction_refuses_to_publish_an_oversized_snapshot() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = CheckpointStore::open(StoreOptions {
        max_tracked_files: 2,
        fingerprint_bytes: 16,
        ..options(&path)
    })
    .expect("namespace opens");

    let _registered = store
        .register_files(vec![widest_registration(1), widest_registration(2)])
        .expect("registers");
    let encoded =
        encode_snapshot(1, &store.table().snapshot_records()).expect("the current table encodes");
    store.limits.max_snapshot_bytes =
        u64::try_from(encoded.len() - 1).expect("snapshot length fits u64");
    let limits = store.limits();

    match store
        .compact()
        .expect_err("an oversized snapshot is refused")
    {
        StoreError::SnapshotTooLarge {
            generation,
            records,
            len,
            max,
            ..
        } => {
            assert_eq!(generation, 1);
            assert_eq!(records, 2);
            assert!(len > max);
            assert_eq!(max, limits.max_snapshot_bytes);
        }
        other => panic!("expected an oversized snapshot refusal, got {other:?}"),
    }

    // Nothing was staged and nothing was published.
    assert!(!path.join(snapshot_file_name(1)).exists());
    assert!(!path.join(wal_file_name(1)).exists());
    assert_eq!(store.generation(), 0);
    assert!(store.retired_generations().is_empty());
    // The refusal is not a durable-state failure, so the store keeps working.
    let _progressed = store
        .commit_progress(vec![wide_progress(1, 0, 128)])
        .expect("the store is still usable");
    drop(store);

    let reopened = CheckpointStore::open(StoreOptions {
        max_tracked_files: 2,
        fingerprint_bytes: 16,
        ..options(&path)
    })
    .expect("the untouched generation still reopens");
    assert_eq!(reopened.generation(), 0);
    assert_eq!(reopened.table().len(), 2);
}

/// Scenario: a selected snapshot whose header declares two records is
/// reopened after `limits.max_tracked_files` is reduced to one.
/// Guarantees: the header count fails recovery before record decoding and
/// allocation rather than opening or materializing an over-capacity table.
#[test]
fn reduced_tracked_file_limit_fails_recovery_closed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = CheckpointStore::open(StoreOptions {
        max_tracked_files: 2,
        ..options(&path)
    })
    .expect("namespace opens");
    let _registered = store
        .register_files(vec![registration(1), registration(2)])
        .expect("registers");
    store
        .compact()
        .expect("the records are moved into the selected snapshot");
    drop(store);

    let error = CheckpointStore::open(StoreOptions {
        max_tracked_files: 1,
        ..options(&path)
    })
    .expect_err("the reduced population fails closed");
    assert!(matches!(
        error,
        StoreError::RecoveredTrackedFilesExceedMaximum {
            tracked: 2,
            max: 1,
            ..
        }
    ));
}

/// Scenario: fingerprint bytes wider than the configured identity window
/// are supplied on registration and then encountered after a limit
/// reduction during recovery.
/// Guarantees: the write path never creates state outside its derived
/// snapshot/WAL formulas, and a narrower reload fails closed instead of
/// accepting durable matching evidence it no longer permits.
#[test]
fn configured_fingerprint_limit_is_enforced_on_write_and_recovery() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = CheckpointStore::open(StoreOptions {
        fingerprint_bytes: 8,
        ..options(&path)
    })
    .expect("namespace opens");
    let mut too_wide = registration(1);
    too_wide.fingerprint.push(0xAA);
    let error = store
        .register_files(vec![too_wide])
        .expect_err("an over-window registration is refused");
    assert!(matches!(
        error,
        StoreError::FingerprintExceedsConfiguredMaximum {
            context: "registration fingerprint",
            len: 9,
            max: 8,
            ..
        }
    ));

    let _registered = store
        .register_files(vec![registration(1)])
        .expect("an in-window fingerprint registers");
    drop(store);

    let error = CheckpointStore::open(StoreOptions {
        fingerprint_bytes: 7,
        ..options(&path)
    })
    .expect_err("the reduced fingerprint window fails closed");
    assert!(matches!(
        error,
        StoreError::FingerprintExceedsConfiguredMaximum {
            context: "recovered fingerprint",
            len: 8,
            max: 7,
            ..
        }
    ));
}

/// Scenario: Windows compacts an already initialized namespace, replacing
/// an existing `CURRENT` marker, then reopens it.
/// Guarantees: the Windows publication path uses supported replacement
/// semantics when the destination exists and the newly selected generation
/// remains recoverable.
#[cfg(windows)]
#[test]
fn windows_compaction_atomically_replaces_current() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = open(&path);
    let _registered = store
        .register_files(vec![registration(1)])
        .expect("registers");
    store.compact().expect("CURRENT replacement succeeds");
    assert_eq!(store.generation(), 1);
    drop(store);

    let reopened = open(&path);
    assert_eq!(reopened.generation(), 1);
    assert_eq!(reopened.table().len(), 1);
}

/// Scenario: Windows reports each documented `ReplaceFileW` failure class
/// that can occur after one or both pathnames have already changed.
/// Guarantees: errors 1176 and 1177 are classified as authority-uncertain,
/// while ordinary failures remain retryable against the current generation.
#[cfg(windows)]
#[test]
fn windows_replace_failure_classifies_uncertain_authority() {
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_UNABLE_TO_MOVE_REPLACEMENT, ERROR_UNABLE_TO_MOVE_REPLACEMENT_2,
        ERROR_UNABLE_TO_REMOVE_REPLACED,
    };

    assert!(super::fsio::windows_replace_failure_may_have_changed(
        i32::try_from(ERROR_UNABLE_TO_MOVE_REPLACEMENT).ok()
    ));
    assert!(super::fsio::windows_replace_failure_may_have_changed(
        i32::try_from(ERROR_UNABLE_TO_MOVE_REPLACEMENT_2).ok()
    ));
    assert!(!super::fsio::windows_replace_failure_may_have_changed(
        i32::try_from(ERROR_UNABLE_TO_REMOVE_REPLACED).ok()
    ));
    assert!(!super::fsio::windows_replace_failure_may_have_changed(
        i32::try_from(ERROR_ACCESS_DENIED).ok()
    ));
    assert!(!super::fsio::windows_replace_failure_may_have_changed(None));
}

/// Scenario: a Windows checkpoint namespace and its publication artifacts
/// have absolute paths longer than the legacy `MAX_PATH` limit.
/// Guarantees: initial publication, `CURRENT` replacement, cleanup, and
/// reopen use extended-length paths and preserve the compacted generation.
#[cfg(windows)]
#[test]
fn windows_long_path_namespace_compacts_and_reopens() {
    use std::os::windows::ffi::OsStrExt as _;

    use windows_sys::Win32::Foundation::MAX_PATH;

    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir
        .path()
        .join("a".repeat(120))
        .join("b".repeat(120))
        .join("namespace");
    let marker_path = path.join(CURRENT_FILE_NAME);
    assert!(
        marker_path.as_os_str().encode_wide().count()
            > usize::try_from(MAX_PATH).expect("MAX_PATH fits usize")
    );

    let mut store = open(&path);
    let _registered = store
        .register_files(vec![registration(1)])
        .expect("registers");
    store.compact().expect("long-path compaction succeeds");
    store
        .cleanup_retired_generations()
        .expect("long-path cleanup succeeds");
    drop(store);

    let reopened = open(&path);
    assert_eq!(reopened.generation(), 1);
    assert_eq!(reopened.table().len(), 1);
}

/// Scenario: another Windows handle keeps `CURRENT` open without delete
/// sharing while compaction tries to replace it.
/// Guarantees: the sharing violation is surfaced before authority changes,
/// the existing generation remains usable and recoverable, and retrying
/// after the conflicting handle closes succeeds.
#[cfg(windows)]
#[test]
fn windows_marker_sharing_violation_preserves_the_old_generation() {
    use std::os::windows::fs::OpenOptionsExt as _;

    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;

    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = open(&path);
    let _registered = store
        .register_files(vec![registration(1)])
        .expect("registers");
    let marker = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path.join(CURRENT_FILE_NAME))
        .expect("marker opens without delete sharing");

    assert!(matches!(store.compact(), Err(StoreError::Io { .. })));
    assert_eq!(store.generation(), 0);
    assert_eq!(committed_offset(&store, 1), 0);
    drop(marker);

    store
        .compact()
        .expect("retry succeeds once sharing permits");
    assert_eq!(store.generation(), 1);
    drop(store);
    assert_eq!(open(&path).generation(), 1);
}

/// Scenario: cleanup of a retired generation is interrupted before unlink,
/// between its two unlinks, or before the final directory sync.
/// Guarantees: the complete pending list remains recorded through every
/// failure, including when both entries are gone but their unlinks are not
/// yet durable, so a retry counts the generation when its complete cleanup
/// becomes durable rather than undercounting already-unlinked files.
#[test]
fn cleanup_preserves_the_remainder_and_retries_after_a_partial_failure() {
    for point in FaultPoint::CLEANUP {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("namespace");
        let seeded = seeded_namespace(&path);

        let mut store = open(&path);
        store.compact().expect("first compaction succeeds");
        assert_eq!(store.retired_generations(), [0]);
        drop(store);

        let mut store = CheckpointStore::open_with_fault(options(&path), point)
            .expect("opening an existing namespace publishes nothing");
        assert_eq!(store.retired_generations(), [0]);

        let error = store
            .cleanup_retired_generations()
            .expect_err("the armed boundary fails");
        assert!(
            matches!(error, StoreError::InjectedFault { point: fired } if fired == point),
            "expected the injected fault at {point}, got {error:?}"
        );
        assert_eq!(
            store.retired_generations(),
            [0],
            "the remainder was lost after a fault at {point}"
        );
        assert_eq!(store.stats().retired_generations, vec![0]);
        match point {
            FaultPoint::BeforeRetiredGenerationRemoval => {
                assert!(path.join(snapshot_file_name(0)).is_file());
                assert!(path.join(wal_file_name(0)).is_file());
            }
            FaultPoint::AfterRetiredSnapshotRemoval => {
                assert!(!path.join(snapshot_file_name(0)).exists());
                assert!(path.join(wal_file_name(0)).is_file());
            }
            FaultPoint::BeforeRetiredDirectorySync => {
                assert!(!path.join(snapshot_file_name(0)).exists());
                assert!(!path.join(wal_file_name(0)).exists());
            }
            other => panic!("unexpected cleanup boundary {other}"),
        }
        assert!(path.join(snapshot_file_name(1)).is_file());
        assert!(path.join(wal_file_name(1)).is_file());

        let removed = store
            .cleanup_retired_generations()
            .expect("the retry completes the remaining work");
        assert_eq!(
            removed, 1,
            "the retry after a fault at {point} reported the wrong removals"
        );
        assert!(store.retired_generations().is_empty());
        assert!(store.stats().retired_generations.is_empty());
        assert!(!path.join(snapshot_file_name(0)).exists());
        assert!(!path.join(wal_file_name(0)).exists());
        assert!(path.join(snapshot_file_name(1)).is_file());
        assert!(path.join(wal_file_name(1)).is_file());
        assert_eq!(records(&store), seeded);
        drop(store);

        let reopened = open(&path);
        assert_eq!(reopened.generation(), 1);
        assert!(reopened.recovery().retired_generations.is_empty());
        assert_eq!(records(&reopened), seeded);
    }
}

/// Scenario: a second compaction is requested before the preceding
/// generation has been durably removed.
/// Guarantees: the store refuses to grow the retired-generation population,
/// stages no additional generation, and allows compaction once cleanup
/// succeeds.
#[test]
fn compaction_requires_retired_generation_cleanup() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = open(&path);

    store.compact().expect("first compaction succeeds");
    let error = store
        .compact()
        .expect_err("a second compaction requires cleanup");
    assert!(matches!(
        error,
        StoreError::RetiredGenerationCleanupRequired { generation: 0, .. }
    ));
    assert_eq!(store.generation(), 1);
    assert_eq!(store.retired_generations(), [0]);
    assert!(!path.join(snapshot_file_name(2)).exists());
    assert!(!path.join(wal_file_name(2)).exists());

    assert_eq!(
        store
            .cleanup_retired_generations()
            .expect("cleanup succeeds"),
        1
    );
    store.compact().expect("compaction resumes after cleanup");
    assert_eq!(store.generation(), 2);
    assert_eq!(store.retired_generations(), [1]);
}

/// Scenario: the reserved reason code `0x0000` supplied to every public
/// path that can encode a `quarantine_file.reason_code` or a
/// `remove_file.removal_reason`, including the raw transaction API and an
/// administrative removal of a record that is not present.
/// Guarantees: the format's "an encoder MUST NOT write 0x0000" rule is
/// enforced once, at the single append funnel plus the two paths that can
/// return early, so no entry point can persist a reserved reason code and
/// none of the refusals advances durable or in-memory state.
#[test]
fn reserved_reason_codes_are_refused_on_every_public_path() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = open(&path);
    let _registered = store
        .register_files(vec![registration(1), registration(2)])
        .expect("registers");
    let _quarantined = store
        .quarantine_files(vec![quarantine(2)])
        .expect("quarantine succeeds");
    let bytes_before = store.stats().wal_bytes;
    let sequence_before = store.stats().next_sequence;

    let mut reserved_quarantine = quarantine(1);
    reserved_quarantine.reason_code = REASON_CODE_RESERVED;
    let expect_quarantine_field = |error: StoreError| match error {
        StoreError::ReservedReasonCode { field } => {
            assert_eq!(field, "quarantine_file.reason_code");
        }
        other => panic!("expected a reserved reason code refusal, got {other:?}"),
    };
    expect_quarantine_field(
        store
            .quarantine_files(vec![reserved_quarantine.clone()])
            .expect_err("quarantine_files refuses the reserved code"),
    );
    expect_quarantine_field(
        store
            .append(vec![Operation::QuarantineFile(reserved_quarantine)])
            .expect_err("append refuses the reserved code"),
    );

    let expect_removal_field = |error: StoreError| match error {
        StoreError::ReservedReasonCode { field } => {
            assert_eq!(field, "remove_file.removal_reason");
        }
        other => panic!("expected a reserved reason code refusal, got {other:?}"),
    };
    expect_removal_field(
        store
            .remove_files(vec![removal(1, REASON_CODE_RESERVED)])
            .expect_err("remove_files refuses the reserved code"),
    );
    expect_removal_field(
        store
            .append(vec![Operation::RemoveFile(removal(
                1,
                REASON_CODE_RESERVED,
            ))])
            .expect_err("append refuses the reserved code"),
    );
    expect_removal_field(
        store
            .remove_expired(
                &HashSet::new(),
                10_000_000_000,
                Duration::from_secs(1),
                REASON_CODE_RESERVED,
            )
            .expect_err("remove_expired refuses the reserved code"),
    );
    // Refused even where the record is absent, which would otherwise be
    // reported as the format's idempotent no-op.
    expect_removal_field(
        store
            .remove_quarantined_file(
                file_id(9),
                REASON_CODE_RESERVED,
                10_000_000_000,
                "operator purge".to_owned(),
            )
            .expect_err("remove_quarantined_file refuses the reserved code"),
    );
    expect_removal_field(
        store
            .remove_quarantined_file(
                file_id(2),
                REASON_CODE_RESERVED,
                10_000_000_000,
                "operator purge".to_owned(),
            )
            .expect_err("remove_quarantined_file refuses the reserved code"),
    );

    assert_eq!(store.stats().wal_bytes, bytes_before);
    assert_eq!(store.stats().next_sequence, sequence_before);
    assert_eq!(store.table().len(), 2);

    // A non-reserved code on the same paths still works.
    let _removed = store
        .remove_files(vec![removal(1, 0x0007)])
        .expect("a valid removal reason is accepted");
    let outcome = store
        .remove_quarantined_file(file_id(2), 0x0008, 10_000_000_000, "purge".to_owned())
        .expect("a valid administrative removal is accepted")
        .expect("the record was present");
    assert!(outcome.synced);
    assert_eq!(store.table().len(), 0);
}

/// Scenario: a checksum-valid WAL contains either a quarantine or removal
/// operation with the reason code encoders reserve.
/// Guarantees: recovery rejects both operations before applying them, so a
/// reserved removal cannot evade validation by deleting its own evidence
/// from the final table.
#[test]
fn recovered_wal_rejects_every_reserved_reason_code() {
    let mut quarantine = quarantine(1);
    quarantine.reason_code = REASON_CODE_RESERVED;
    for (name, operation, expected_field) in [
        (
            "quarantine",
            Operation::QuarantineFile(quarantine),
            "quarantine_file.reason_code",
        ),
        (
            "removal",
            Operation::RemoveFile(removal(1, REASON_CODE_RESERVED)),
            "remove_file.removal_reason",
        ),
    ] {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(name);
        let mut store = open(&path);
        let _registered = store
            .register_files(vec![registration(1)])
            .expect("registers");
        drop(store);

        let transaction = Transaction {
            sequence: 2,
            operations: vec![operation],
        }
        .encode()
        .expect("the structurally valid transaction encodes");
        let wal_path = path.join(wal_file_name(0));
        let mut wal = fs::OpenOptions::new()
            .append(true)
            .open(&wal_path)
            .expect("WAL opens");
        wal.write_all(&transaction)
            .expect("transaction is appended");
        wal.sync_all().expect("transaction is durable");
        drop(wal);

        let error =
            CheckpointStore::open(options(&path)).expect_err("the reserved code fails recovery");
        assert!(matches!(
            error,
            StoreError::ReservedReasonCodeRecovered {
                file_id: found,
                field,
                ..
            } if found == file_id(1) && field == expected_field
        ));
    }
}

/// Scenario: a checksum-valid snapshot contains reserved quarantine evidence
/// and its WAL immediately resets or administratively removes that record.
/// Guarantees: recovery validates the snapshot before replay, so a later
/// valid operation cannot erase the forbidden durable value before it is
/// reported.
#[test]
fn recovered_snapshot_rejects_reserved_reason_before_wal_can_erase_it() {
    let reset = Operation::ResetQuarantinedFile(ResetQuarantinedFile {
        file_id: file_id(1),
        expected_quarantine_epoch: 1,
        action: ResetQuarantineAction::ResetToBeginning,
        resulting_epoch: 2,
        resulting_offset: 0,
        new_framing_resume: FramingResume::Clean,
        reset_time_unix_nano: 4_000,
        audit_reason: "operator reset".to_owned(),
    });
    let removal = Operation::RemoveFile(RemoveFile {
        file_id: file_id(1),
        expected_file_epoch: 1,
        expected_prior_state: LifecycleState::Quarantined,
        removal_reason: 0x0001,
        removal_time_unix_nano: 4_000,
        administrative: true,
        namespace_id: Some(NAMESPACE_ID.to_owned()),
        audit_reason: Some("operator removal".to_owned()),
    });

    for (name, operation) in [("reset", reset), ("removal", removal)] {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(name);
        let mut store = open(&path);
        let _registered = store
            .register_files(vec![registration(1)])
            .expect("registers");
        let _quarantined = store
            .quarantine_files(vec![quarantine(1)])
            .expect("quarantines");
        store.compact().expect("quarantine snapshot is published");
        let mut snapshot_records = records(&store);
        snapshot_records[0]
            .quarantine_evidence
            .as_mut()
            .expect("the snapshot record is quarantined")
            .reason_code = REASON_CODE_RESERVED;
        drop(store);

        write_bytes(
            &path.join(snapshot_file_name(1)),
            &encode_snapshot(1, &snapshot_records).expect("the snapshot encodes"),
        );
        let transaction = Transaction {
            sequence: 1,
            operations: vec![operation],
        }
        .encode()
        .expect("the erasing transaction encodes");
        let wal_path = path.join(wal_file_name(1));
        let mut wal = fs::OpenOptions::new()
            .append(true)
            .open(&wal_path)
            .expect("WAL opens");
        wal.write_all(&transaction)
            .expect("transaction is appended");
        wal.sync_all().expect("transaction is durable");
        drop(wal);

        let error = CheckpointStore::open(options(&path))
            .expect_err("the snapshot's reserved code fails recovery");
        assert!(matches!(
            error,
            StoreError::ReservedReasonCodeRecovered {
                file_id: found,
                field: "quarantine_file.reason_code",
                ..
            } if found == file_id(1)
        ));
    }
}

/// Scenario: a fresh subprocess recovers a snapshot of wide records, using
/// 10,000 records by default or `OTAP_FILELOG_CHECKPOINT_STRESS_RECORDS`
/// when a boundary measurement is requested.
/// Guarantees: snapshot recovery preserves every record and emits
/// reproducible elapsed-time, modeled working-set, and sampled peak-RSS
/// evidence without fixture-construction allocations contaminating the
/// measured process.
#[test]
#[ignore = "resource-intensive checkpoint recovery measurement"]
fn checkpoint_recovery_stress_reports_latency_and_peak_memory() {
    const CHILD_PATH_ENV: &str = "OTAP_FILELOG_CHECKPOINT_STRESS_CHILD_PATH";
    const REPORT_PATH_ENV: &str = "OTAP_FILELOG_CHECKPOINT_STRESS_REPORT_PATH";
    const RECORDS_ENV: &str = "OTAP_FILELOG_CHECKPOINT_STRESS_RECORDS";
    const TEST_NAME: &str = "receivers::filelog_receiver::checkpoint::store::tests::\
                             checkpoint_recovery_stress_reports_latency_and_peak_memory";
    let records = std::env::var(RECORDS_ENV)
        .map(|value| {
            value
                .parse::<usize>()
                .expect("stress record count is valid")
        })
        .unwrap_or(10_000);
    let max_tracked_files = u32::try_from(records).expect("stress record count fits u32");

    if let Some(path) = std::env::var_os(CHILD_PATH_ENV) {
        let path = PathBuf::from(path);
        let options = StoreOptions {
            max_tracked_files,
            fingerprint_bytes: 16,
            ..options(&path)
        };
        let peak_before = peak_resident_set_bytes();
        let sampling = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let peak = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(peak_before.unwrap_or(0)));
        let sampler_running = std::sync::Arc::clone(&sampling);
        let sampler_peak = std::sync::Arc::clone(&peak);
        let sampler = std::thread::spawn(move || {
            while sampler_running.load(std::sync::atomic::Ordering::Relaxed) {
                if let Some(resident) = peak_resident_set_bytes() {
                    let _previous =
                        sampler_peak.fetch_max(resident, std::sync::atomic::Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let started = Instant::now();
        let store = CheckpointStore::open(options).expect("stress namespace recovers");
        let elapsed = started.elapsed();
        if let Some(resident) = peak_resident_set_bytes() {
            let _previous = peak.fetch_max(resident, std::sync::atomic::Ordering::Relaxed);
        }
        sampling.store(false, std::sync::atomic::Ordering::Relaxed);
        sampler.join().expect("the RSS sampler joins");
        let peak_after = peak.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(store.table().len(), records);
        assert_eq!(store.recovery().snapshot_records, records);
        assert_eq!(store.recovery().transactions_replayed, 0);
        let generation = store.generation();
        let report = format!(
            "records={records} snapshot_bytes={} wal_bytes={} \
             modeled_working_bytes={} elapsed_micros={} peak_rss_before_bytes={} \
             peak_rss_after_bytes={} peak_rss_delta_bytes={}\n",
            fs::metadata(path.join(snapshot_file_name(generation)))
                .expect("snapshot metadata")
                .len(),
            fs::metadata(path.join(wal_file_name(generation)))
                .expect("WAL metadata")
                .len(),
            store.limits().max_recovery_working_bytes,
            elapsed.as_micros(),
            peak_before.unwrap_or(0),
            peak_after,
            peak_after.saturating_sub(peak_before.unwrap_or(0)),
        );
        let report_path = PathBuf::from(
            std::env::var_os(REPORT_PATH_ENV).expect("the child report path is configured"),
        );
        fs::write(report_path, report).expect("the child writes its measurement");
        return;
    }

    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let store_options = StoreOptions {
        max_tracked_files,
        fingerprint_bytes: 16,
        ..options(&path)
    };
    let mut store = CheckpointStore::open(store_options).expect("namespace opens");
    for start in (0..records).step_by(usize::from(WAL_MAX_OPS_PER_TX)) {
        let end = start
            .checked_add(usize::from(WAL_MAX_OPS_PER_TX))
            .unwrap_or(records)
            .min(records);
        let registrations: Vec<RegisterFile> = (start as u64..end as u64)
            .map(widest_registration)
            .collect();
        let _outcomes = store
            .register_files(registrations)
            .expect("stress records register");
        if store.compaction_due() {
            store.compact().expect("intermediate snapshot is published");
            let _removed = store
                .cleanup_retired_generations()
                .expect("intermediate generation is cleaned");
        }
    }
    if store.stats().wal_transactions > 0 {
        store.compact().expect("final snapshot is published");
        let _removed = store
            .cleanup_retired_generations()
            .expect("final retired generation is cleaned");
    }
    drop(store);

    let report_path = dir.path().join("recovery-report.txt");
    let status = std::process::Command::new(std::env::current_exe().expect("test binary path"))
        .args(["--ignored", "--exact", TEST_NAME, "--nocapture"])
        .env(CHILD_PATH_ENV, &path)
        .env(REPORT_PATH_ENV, &report_path)
        .status()
        .expect("recovery measurement child starts");
    assert!(status.success(), "recovery measurement child failed");
    let report = fs::read_to_string(report_path).expect("the child produced its report");
    eprintln!("filelog checkpoint recovery measurement: {report}");
}

/// Scenario: store options built with `StoreOptions::from_runtime_config`
/// from validated receiver configurations -- the shipped defaults, the
/// largest recoverable fingerprint window and tracked-file
/// population, and the largest recoverable compaction threshold -- each
/// used to create, write to, and reopen a namespace.
/// Guarantees: options carry the resolved namespace identity and all three
/// size knobs from one validated configuration, and every configuration
/// that validates can create a namespace, write to it, and recover it, so
/// no legal configuration -- including each size boundary -- produces a
/// namespace its own store cannot reopen.
#[test]
fn every_legal_configuration_creates_a_namespace_that_reopens() {
    let default_tracked_files = LimitsConfig::default().max_tracked_files;
    let default_fingerprint_bytes = IdentityConfig::default().fingerprint_bytes;
    let default_compact_after_bytes = CheckpointConfig::default().compact_after_bytes;
    let boundary_files = u32::try_from(largest_accepted(
        u64::from(default_tracked_files),
        u64::from(u32::MAX),
        |candidate| {
            StoreLimits::derive(
                default_compact_after_bytes,
                candidate as u32,
                default_fingerprint_bytes,
            )
            .is_ok()
        },
    ))
    .expect("the tracked-file boundary fits u32");
    let boundary_bytes = largest_accepted(
        default_compact_after_bytes,
        ARTIFACT_BYTES_CEILING,
        |candidate| {
            StoreLimits::derive(candidate, default_tracked_files, default_fingerprint_bytes).is_ok()
        },
    );
    let boundary_fingerprint = largest_accepted(
        default_fingerprint_bytes,
        FINGERPRINT_MAX_BYTES as u64,
        |candidate| {
            StoreLimits::derive(
                default_compact_after_bytes,
                default_tracked_files,
                candidate,
            )
            .is_ok()
        },
    );

    let cases: Vec<(&str, Box<dyn Fn(&mut Config)>)> = vec![
        ("defaults", Box::new(|_: &mut Config| {})),
        (
            "largest-recoverable-fingerprint",
            Box::new(move |config: &mut Config| {
                config.identity.fingerprint_bytes = boundary_fingerprint;
                // Isolate the durable-store boundary from the independent
                // discovery/identity runtime-memory ceiling.
                config.limits.max_pending_candidates = 1;
                config.limits.max_open_files = 1;
            }),
        ),
        (
            "largest-tracked-population",
            Box::new(move |config: &mut Config| {
                config.limits.max_tracked_files = boundary_files;
            }),
        ),
        (
            "largest-compaction-threshold",
            Box::new(move |config: &mut Config| {
                config.checkpoint.compact_after_bytes = boundary_bytes;
            }),
        ),
    ];

    let dir = tempfile::tempdir().expect("temp dir");
    for (name, mutate) in cases {
        let mut config = Config {
            include: vec!["/var/log/app/*.log".to_owned()],
            ..Config::default()
        };
        mutate(&mut config);
        let runtime = RuntimeConfig::from_config(config, NAMESPACE_ID)
            .unwrap_or_else(|error| panic!("the {name} configuration must validate: {error}"));

        let path = dir.path().join(name);
        let options = StoreOptions {
            // Only the namespace location is redirected into the test's
            // temporary directory; every bound comes from the validated
            // configuration.
            namespace_dir: path.clone(),
            ownership_timeout: Duration::from_millis(200),
            ownership_retry_interval: Duration::from_millis(10),
            ..StoreOptions::from_runtime_config(&runtime)
        };
        assert_eq!(options.namespace_id, NAMESPACE_ID);
        assert_eq!(
            options.compact_after_bytes,
            runtime.checkpoint.compact_after_bytes
        );
        assert_eq!(options.max_tracked_files, runtime.limits.max_tracked_files);
        assert_eq!(
            options.fingerprint_bytes,
            runtime.identity.fingerprint_bytes
        );

        let mut store = CheckpointStore::open(options.clone())
            .unwrap_or_else(|error| panic!("the {name} configuration must open: {error}"));
        let limits = store.limits();
        assert!(limits.max_snapshot_bytes <= ARTIFACT_BYTES_CEILING);
        assert!(limits.max_wal_bytes <= ARTIFACT_BYTES_CEILING);
        assert!(limits.max_recovery_working_bytes <= RECOVERY_WORKING_BYTES_CEILING);
        let _registered = store
            .register_files(vec![registration(1)])
            .unwrap_or_else(|error| panic!("the {name} configuration must register: {error}"));
        let _progressed = store
            .commit_progress(vec![progress(1, 0, 512)])
            .unwrap_or_else(|error| panic!("the {name} configuration must progress: {error}"));
        store.compact().expect("compaction succeeds");
        drop(store);

        let reopened = CheckpointStore::open(options)
            .unwrap_or_else(|error| panic!("the {name} configuration must reopen: {error}"));
        assert_eq!(reopened.generation(), 1);
        assert_eq!(committed_offset(&reopened, 1), 512);
        assert_eq!(reopened.limits(), limits);
    }
}
