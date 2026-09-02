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

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::super::DecodeError;
use super::super::primitives::{
    ADVISORY_PATH_STORED_MAX_BYTES, AdvisoryPath, CommittedFrontierGuard, FINGERPRINT_MAX_BYTES,
    FileId, FramingResume, LifecycleState, Locator, NAMESPACE_ID_MAX_BYTES, REASON_CODE_RESERVED,
    TRUNCATE_RESET_REASON_READ_NEW, WAL_MAX_NON_PROGRESS_OPS_PER_TX, WAL_MAX_OPS_PER_TX, crc32c,
};
use super::super::snapshot::{SNAPSHOT_HEADER_LEN, SnapshotRecord, encode_snapshot};
use super::super::wal::{
    Operation, QuarantineFile, RegisterFile, RemoveFile, ResetAfterTruncate, ResetQuarantineAction,
    ResetQuarantinedFile, Transaction, TransactionScan, UpdateProgress, WAL_HEADER_LEN,
    encode_transaction, encode_wal_header, scan_next_transaction,
};
use super::error::StoreError;
use super::fault::{FaultPlan, FaultPoint};
use super::layout::{
    CURRENT_COMPACT_TEMP_FILE_NAME, CURRENT_CREATE_TEMP_FILE_NAME, CURRENT_FILE_NAME,
    MAX_GENERATIONS_ON_DISK, MAX_TEMP_FILES, OWNERSHIP_LOCK_FILE_NAME, PublicationRole,
    snapshot_file_name, temp_file_name, wal_file_name,
};
use super::limits::{
    ARTIFACT_BYTES_CEILING, RECOVERY_WORKING_BYTES_CEILING, StoreLimits,
    minimum_compact_after_bytes,
};
use super::{AtomicGroupAppendOutcome, CheckpointStore, StoreOptions};

/// Test-only zero-filled window guard: a deterministic, obviously-fake
/// `CommittedFrontierGuard` for tests that only need a structurally valid
/// guard and do not exercise real continuity evidence. Production code
/// must never do this; see [`super::super::primitives::CommittedFrontierWindow`]
/// for the real, non-fabricated runtime window.
fn zero_guard(committed_offset: u64) -> CommittedFrontierGuard {
    let window_len = committed_offset.min(64) as usize;
    CommittedFrontierGuard::compute(committed_offset, &vec![0u8; window_len]).unwrap()
}

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
    assert_eq!(store.stats().wal_transactions, 2);
}

/// Scenario: one Ack-driven progress transaction remains unsynced for a
/// deterministic interval before a successful explicit sync.
/// Guarantees: sync-delay telemetry starts at the first unsynced progress,
/// records exactly once after successful sync, and an empty repeated sync
/// cannot duplicate the observation.
#[test]
fn sync_delay_measures_first_unsynced_progress_to_successful_sync() {
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
    assert_eq!(store.stats().sync_delay_operations, 0);
    store.first_unsynced_at = Some(Instant::now() - Duration::from_millis(10));

    store.sync().expect("outstanding progress syncs");
    let synced = store.stats();
    assert_eq!(synced.sync_delay_operations, 1);
    assert!(synced.sync_delay_ns >= 10_000_000);

    store.sync().expect("empty repeated sync is a no-op");
    let repeated = store.stats();
    assert_eq!(repeated.sync_delay_operations, 1);
    assert_eq!(repeated.sync_delay_ns, synced.sync_delay_ns);
}

/// Scenario: a zero-interval Ack progress append completes, its required WAL
/// sync fails, and the exact transaction is retried after the fault clears.
/// Guarantees: current WAL size/count reflect the known-complete pending frame,
/// and successful retry records one immediate sync-delay observation without
/// losing the original append start.
#[test]
fn sync_failure_preserves_physical_wal_gauges_and_delay_start() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = CheckpointStore::open(StoreOptions {
        sync_interval: Duration::ZERO,
        ..options(&path)
    })
    .expect("namespace opens");
    let _registered = store
        .register_files(vec![registration(1)])
        .expect("registers");
    let before = store.stats();
    store.faults = FaultPlan::armed(FaultPoint::BeforeWalSync);

    assert!(matches!(
        store.commit_progress(vec![progress(1, 0, 128)]),
        Err(StoreError::InjectedFault {
            point: FaultPoint::BeforeWalSync
        })
    ));
    let pending = store
        .pending_wal_append
        .expect("the complete append remains pending required sync");
    assert!(pending.requires_sync);
    let failed = store.stats();
    assert_eq!(failed.wal_bytes, pending.wal_bytes_after);
    assert_eq!(failed.wal_transactions, pending.wal_transactions_after);
    assert!(failed.wal_bytes > before.wal_bytes);
    assert_eq!(failed.wal_transactions, before.wal_transactions + 1);
    assert_eq!(failed.sync_delay_operations, 0);

    store.faults = FaultPlan::disabled();
    let retried = store
        .commit_progress(vec![progress(1, 0, 128)])
        .expect("the exact pending append reconciles and syncs");
    assert!(retried[0].synced);
    let synced = store.stats();
    assert_eq!(synced.wal_transactions, pending.wal_transactions_after);
    assert_eq!(synced.sync_delay_operations, 1);
}

/// Scenario: deferred Ack progress is made durable by compaction, then a new
/// generation receives another deferred progress transaction.
/// Guarantees: compaction consumes the old sync-delay timer and the next
/// generation starts a fresh observation rather than spanning generations.
#[test]
fn compaction_consumes_sync_delay_before_new_generation() {
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
        .commit_progress(vec![progress(1, 0, 64)])
        .expect("progress is deferred");
    store.first_unsynced_at = Some(Instant::now() - Duration::from_millis(5));

    store
        .compact()
        .expect("compaction syncs the old generation");
    assert_eq!(store.stats().sync_delay_operations, 1);
    assert!(store.first_unsynced_at.is_none());

    let _progressed = store
        .commit_progress(vec![progress(1, 64, 128)])
        .expect("new-generation progress is deferred");
    assert!(store.first_unsynced_at.is_some());
    store.sync().expect("new generation syncs");
    assert_eq!(store.stats().sync_delay_operations, 2);
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
    assert_eq!(store.stats().sync_delay_operations, 1);
    assert!(store.next_sync_deadline().is_none());
}

fn open(dir: &Path) -> CheckpointStore {
    CheckpointStore::open(options(dir)).expect("namespace opens")
}

/// Scenario: `CURRENT` is missing and the only surviving half of initial
/// generation 0 declares another generation, or is a WAL with a torn tail.
/// Guarantees: exact first-publication names are treated as unassigned
/// interrupted output, removed without decoding, and replaced by a fresh
/// empty generation zero.
#[test]
fn markerless_recognized_initial_artifacts_are_replaced_without_decoding() {
    let dir = tempfile::tempdir().expect("temp dir");

    let foreign = dir.path().join("foreign");
    drop(open(&foreign));
    fs::remove_file(foreign.join(CURRENT_FILE_NAME)).expect("removes the marker");
    fs::remove_file(foreign.join(wal_file_name(0))).expect("removes the WAL");
    write_bytes(
        &foreign.join(snapshot_file_name(0)),
        &encode_snapshot(9, NAMESPACE_ID, &[]).expect("foreign snapshot encodes"),
    );
    let reopened = CheckpointStore::open(options(&foreign)).expect("publication restarts");
    assert!(reopened.recovery().created);
    assert!(reopened.table().is_empty());
    drop(reopened);

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
    let reopened = CheckpointStore::open(options(&torn)).expect("publication restarts");
    assert!(reopened.recovery().created);
    assert!(reopened.table().is_empty());
}

fn file_id(seed: u8) -> FileId {
    FileId::from_bytes([seed; 16])
}

/// Scenario: a runtime-vetted set names one finalized record together with a
/// quarantined record whose persisted last-seen time is equally old.
/// Guarantees: durable wall-clock age does not select retention, and the
/// store rejects the complete invalid set rather than partially removing the
/// non-quarantined record.
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

    let invalid = HashSet::from([file_id(2), file_id(3)]);
    let error = store.remove_vetted_retention_records(&invalid).unwrap_err();
    assert!(matches!(
        error,
        StoreError::RetentionCandidateQuarantined {
            file_id: quarantined
        } if quarantined == file_id(2)
    ));
    assert_eq!(store.table().len(), 3);

    let missing = HashSet::from([file_id(3), file_id(9)]);
    let error = store.remove_vetted_retention_records(&missing).unwrap_err();
    assert!(matches!(
        error,
        StoreError::RetentionCandidateMissing {
            file_id: absent
        } if absent == file_id(9)
    ));
    assert_eq!(store.table().len(), 3);
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
            new_committed_frontier_guard: zero_guard(0),
            new_framing_resume: FramingResume::Clean,
            new_fingerprint: vec![1; 8],
            action_time_unix_nano: 4_000,
            namespace_id: NAMESPACE_ID.to_owned(),
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
        committed_frontier_guard: CommittedFrontierGuard::empty(),
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
        advisory_path: AdvisoryPath::from_unix_bytes(format!("/var/log/app-{seed}.log").as_bytes())
            .unwrap(),
    }
}

fn distinct_registrations(count: usize) -> Vec<RegisterFile> {
    (0..count)
        .map(|index| RegisterFile {
            file_id: wide_file_id(index as u64),
            locator: Locator::PosixDevIno {
                dev: 7,
                ino: index as u64 + 1,
            },
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
    let leading = vec![operation.clone(); usize::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX) - 1];
    let pair = vec![operation.clone(), operation];

    let (operations, transaction_lengths) =
        super::pack_atomic_groups(vec![leading, pair]).expect("groups pack");

    assert_eq!(
        operations.len(),
        usize::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX) + 1
    );
    assert_eq!(
        transaction_lengths,
        vec![usize::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX) - 1, 2]
    );
}

/// Scenario: a grouped identity plan spans two maximum-sized WAL
/// transactions and cancellation becomes visible after the first append.
/// Guarantees: no second transaction starts, the explicit outcome identifies
/// one completed durable prefix, and restart recovers exactly that prefix.
#[test]
fn atomic_group_cancellation_stops_before_the_next_transaction() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store_options = options(&path);
    store_options.max_tracked_files = u32::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX) + 1;
    let registrations = distinct_registrations(usize::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX) + 1);
    let final_file_id = registrations.last().expect("final registration").file_id;
    let groups = registrations
        .into_iter()
        .map(|registration| vec![Operation::RegisterFile(registration)])
        .collect();
    let cancellation_checks = std::cell::Cell::new(0usize);
    let mut store = CheckpointStore::open(store_options.clone()).expect("namespace initializes");

    let outcome = store
        .append_atomic_groups_cancellable(groups, || {
            let current = cancellation_checks.get();
            cancellation_checks.set(current + 1);
            current != 0
        })
        .expect("grouped append stops cleanly");

    let AtomicGroupAppendOutcome::Cancelled { completed } = outcome else {
        panic!("expected cancellation after the first transaction");
    };
    assert_eq!(completed.len(), 1);
    assert_eq!(
        completed[0].operations,
        usize::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX)
    );
    assert_eq!(
        store.table().len(),
        usize::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX)
    );
    assert!(store.table().get(&final_file_id).is_none());
    drop(store);

    let recovered = CheckpointStore::open(store_options).expect("namespace recovers");
    assert_eq!(
        recovered.table().len(),
        usize::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX)
    );
    assert!(recovered.table().get(&final_file_id).is_none());
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

/// Scenario: a preflighted grouped plan commits its first full transaction,
/// then the second transaction has an ambiguous complete-write result.
/// Guarantees: the retained plan cursor retries that exact second
/// transaction instead of rebuilding or duplicating the committed prefix.
#[test]
fn atomic_group_plan_resumes_the_exact_failed_transaction() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let count = usize::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX) + 1;
    let store_options = StoreOptions {
        max_tracked_files: count as u32,
        ..options(&path)
    };
    drop(CheckpointStore::open(store_options.clone()).expect("namespace initializes"));
    let groups: Vec<Vec<Operation>> = distinct_registrations(count)
        .into_iter()
        .map(|registration| vec![Operation::RegisterFile(registration)])
        .collect();
    let mut store = CheckpointStore::open_with_fault_after(
        store_options.clone(),
        FaultPoint::AfterWalTransactionWrite,
        1,
    )
    .expect("namespace opens");
    let mut plan = store
        .prepare_atomic_group_append(groups)
        .expect("the complete grouped plan preflights");

    assert!(matches!(
        store
            .append_atomic_group_plan_cancellable(&mut plan, || false)
            .expect_err("the second transaction result is uncertain"),
        StoreError::InjectedFault {
            point: FaultPoint::AfterWalTransactionWrite
        }
    ));
    assert_eq!(plan.next_transaction, 1);
    assert_eq!(
        store.table().len(),
        usize::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX)
    );
    let outcome = store
        .append_atomic_group_plan_cancellable(&mut plan, || false)
        .expect("the retained exact transaction retries");
    assert!(matches!(
        outcome,
        AtomicGroupAppendOutcome::Completed(ref outcomes)
            if outcomes.len() == 1 && outcomes[0].sequence == 2
    ));
    assert_eq!(plan.next_transaction, 2);
    assert_eq!(store.table().len(), count);
    assert_eq!(store.stats().wal_transactions, 2);
    drop(store);

    let reopened = CheckpointStore::open(store_options).expect("complete plan reopens");
    assert_eq!(reopened.table().len(), count);
    assert_eq!(reopened.recovery().transactions_replayed, 2);
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

/// Scenario: pre-append compaction would first sync one deferred transaction,
/// then an immediate registration append would require another sync while the
/// sync counter has room for only one.
/// Guarantees: batched preflight accounts for both syncs and rejects before
/// `CURRENT`, the WAL, or the table changes.
#[test]
fn compaction_and_append_sync_overflow_is_preflighted_before_publication() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = CheckpointStore::open(StoreOptions {
        compact_after_transactions: 2,
        sync_interval: NEVER_ELAPSES,
        ..options(&path)
    })
    .unwrap();
    let _registered = store.register_files(vec![registration(1)]).unwrap();
    let _progressed = store.commit_progress(vec![progress(1, 0, 1)]).unwrap();
    assert_eq!(store.stats().wal_transactions, 2);
    assert_eq!(store.stats().unsynced_transactions, 1);
    store.syncs = u64::MAX - 1;
    let marker_before = fs::read(path.join(CURRENT_FILE_NAME)).unwrap();
    let wal_before = fs::read(path.join(wal_file_name(0))).unwrap();
    let table_before = store.table().clone();

    let error = store.register_files(vec![registration(2)]).unwrap_err();
    assert!(matches!(
        error,
        StoreError::CounterOverflow {
            counter: "WAL syncs",
            ..
        }
    ));
    assert_eq!(store.generation(), 0);
    assert_eq!(store.syncs, u64::MAX - 1);
    assert_eq!(store.stats().preappend_compactions, 0);
    assert_eq!(store.table(), &table_before);
    assert_eq!(
        fs::read(path.join(CURRENT_FILE_NAME)).unwrap(),
        marker_before
    );
    assert_eq!(fs::read(path.join(wal_file_name(0))).unwrap(), wal_before);

    let error = store
        .append(vec![Operation::RegisterFile(registration(2))])
        .unwrap_err();
    assert!(matches!(
        error,
        StoreError::CounterOverflow {
            counter: "WAL syncs",
            ..
        }
    ));
    assert_eq!(store.generation(), 0);
    assert_eq!(store.table(), &table_before);
}

/// Scenario: a three-chunk batch at a one-transaction threshold starts from
/// projected generation `u64::MAX - 1`.
/// Guarantees: preflight detects the second required generation increment
/// before the first chunk writes any WAL bytes or table state.
#[test]
fn batched_preflight_rejects_generation_overflow_before_first_chunk() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let count = usize::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX) * 2 + 1;
    let mut store = CheckpointStore::open(StoreOptions {
        compact_after_transactions: 1,
        max_tracked_files: count as u32,
        ..options(&path)
    })
    .unwrap();
    store.generation = u64::MAX - 1;
    let wal_before = fs::read(path.join(wal_file_name(0))).unwrap();

    let error = store
        .register_files(distinct_registrations(count))
        .unwrap_err();
    assert!(matches!(
        error,
        StoreError::GenerationOverflow {
            generation: u64::MAX
        }
    ));
    assert!(store.table().is_empty());
    assert_eq!(store.stats().wal_transactions, 0);
    assert_eq!(fs::read(path.join(wal_file_name(0))).unwrap(), wal_before);

    store.generation = u64::MAX;
    store.wal_transactions = 1;
    let error = store
        .append(vec![Operation::RegisterFile(registration(1))])
        .unwrap_err();
    assert!(matches!(
        error,
        StoreError::GenerationOverflow {
            generation: u64::MAX
        }
    ));
    assert!(store.table().is_empty());
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

/// Scenario: an initialized Unix namespace and its parent grant their owner
/// search and write permission but not directory reads.
/// Guarantees: reopening repairs the namespace mode before enumerating it and
/// adds no parent-read requirement for incomplete-creation durability work.
#[cfg(unix)]
#[test]
fn established_namespace_reopens_through_execute_only_parent() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().expect("temp dir");
    let parent = dir.path().join("execute-only");
    fs::create_dir(&parent).expect("parent is created");
    let path = parent.join("namespace");
    drop(open(&path));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o300)).expect("namespace mode changes");
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o300)).expect("parent mode changes");

    let reopened = open(&path);
    assert_eq!(reopened.generation(), 0);
    assert_eq!(
        fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
        0o700
    );
    drop(reopened);

    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))
        .expect("parent mode is restored for cleanup");
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

/// Scenario: a markerless Unix namespace contains a FIFO at the exact
/// `CURRENT.create.tmp` publication name and no process has opened its write end.
/// Guarantees: startup opens checkpoint artifacts nonblocking and rejects
/// the special file through handle metadata instead of waiting indefinitely
/// while holding namespace ownership.
#[cfg(unix)]
#[test]
fn unix_fifo_checkpoint_artifact_is_rejected_without_blocking() {
    use std::sync::mpsc;
    use std::thread;

    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    drop(open(&path));
    fs::remove_file(path.join(CURRENT_FILE_NAME)).expect("CURRENT is removed");
    let fifo_path = path.join(CURRENT_CREATE_TEMP_FILE_NAME);
    make_fifo(&fifo_path);

    let (tx, rx) = mpsc::channel();
    let open_path = path.clone();
    let opener = thread::spawn(move || {
        let result = CheckpointStore::open(options(&open_path));
        let rejected = matches!(result, Err(StoreError::UnsafeFilesystemObject { .. }));
        tx.send(rejected).expect("result receiver remains live");
    });

    match rx.recv_timeout(Duration::from_secs(1)) {
        Ok(rejected) => assert!(rejected, "the FIFO must be rejected as unsafe"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let _writer = fs::OpenOptions::new()
                .write(true)
                .open(&fifo_path)
                .expect("a writer unblocks the legacy blocking open");
            opener.join().expect("opener exits after being unblocked");
            panic!("checkpoint startup blocked on the FIFO");
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            opener.join().expect("opener panic is propagated");
            panic!("checkpoint opener disconnected without a result");
        }
    }
    opener.join().expect("opener exits");
}

/// Scenario: a Unix namespace contains a FIFO at the exact
/// `ownership.lock` name.
/// Guarantees: ownership acquisition opens the named object nonblocking and
/// rejects it as non-regular before attempting a lock or waiting for the
/// ownership timeout.
#[cfg(unix)]
#[test]
fn unix_fifo_ownership_lock_is_rejected_without_waiting() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    fs::create_dir(&path).expect("namespace is created");
    let lock_path = path.join(OWNERSHIP_LOCK_FILE_NAME);
    make_fifo(&lock_path);

    let started = Instant::now();
    let error = CheckpointStore::open(options(&path)).expect_err("the FIFO lock is rejected");
    assert!(matches!(
        error,
        StoreError::UnsafeFilesystemObject { path, .. } if path == lock_path
    ));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "special-file rejection must not consume the ownership timeout"
    );
}

/// Scenario: a missing Unix checkpoint namespace has a component one byte
/// larger than the primary targets' 255-byte filesystem limit.
/// Guarantees: startup queries the containing filesystem and reports the
/// component bound before attempting to create an unusable namespace.
#[cfg(unix)]
#[test]
fn unix_namespace_creation_enforces_the_filesystem_component_limit() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("a".repeat(256));

    let error = CheckpointStore::open(options(&path))
        .expect_err("the overlong namespace component is rejected");
    assert!(matches!(
        error,
        StoreError::NamespaceComponentTooLong {
            path: rejected,
            len: 256,
            max,
        } if rejected == path && max < 256
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
        encode_wal_header(0, NAMESPACE_ID).expect("the empty WAL encodes")
    );
}

fn progress(seed: u8, from: u64, to: u64) -> UpdateProgress {
    UpdateProgress {
        file_id: file_id(seed),
        expected_committed_offset: from,
        expected_file_epoch: 1,
        new_committed_offset: to,
        new_committed_frontier_guard: zero_guard(to),
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

#[cfg(unix)]
#[allow(unsafe_code, reason = "libc exposes no safe FIFO constructor")]
fn make_fifo(path: &Path) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt as _;

    let fifo_name = CString::new(path.as_os_str().as_bytes()).expect("path has no NUL");
    // SAFETY: `fifo_name` is a live NUL-terminated path and the mode is a
    // valid permission bitmask.
    let created = unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) };
    assert_eq!(
        created,
        0,
        "FIFO creation failed: {}",
        std::io::Error::last_os_error()
    );
}

fn patch_first_transaction_operation_u16(
    transaction: &mut [u8],
    field_offset_in_payload: usize,
    value: u16,
) {
    const TRANSACTION_HEADER_LEN: usize = 36;
    let operation_start = TRANSACTION_HEADER_LEN;
    let operation_len = u32::from_be_bytes(
        transaction[operation_start..operation_start + 4]
            .try_into()
            .expect("operation length is present"),
    ) as usize;
    let payload_start = operation_start + 4;
    let field_start = payload_start + field_offset_in_payload;
    transaction[field_start..field_start + 2].copy_from_slice(&value.to_be_bytes());

    let operation_crc_start = payload_start + operation_len;
    let operation_crc = crc32c(&transaction[operation_start..operation_crc_start]);
    transaction[operation_crc_start..operation_crc_start + 4]
        .copy_from_slice(&operation_crc.to_be_bytes());

    let transaction_crc_start = transaction.len() - 4;
    let transaction_crc = crc32c(&transaction[..transaction_crc_start]);
    transaction[transaction_crc_start..].copy_from_slice(&transaction_crc.to_be_bytes());
}

fn patch_first_snapshot_quarantine_reason(snapshot: &mut [u8], value: u16) {
    let record_start = SNAPSHOT_HEADER_LEN;
    let record_len = u32::from_be_bytes(
        snapshot[record_start..record_start + 4]
            .try_into()
            .expect("record length is present"),
    ) as usize;
    let payload_start = record_start + 4;
    // This test fixture is the fixed-width Posix/Clean quarantined record
    // produced from `registration(1)`: the reason follows 129 payload bytes.
    const REASON_OFFSET: usize = 16 + 4 + 8 + 34 + 2 + 8 + 4 + 17 + 2 + 32 + 1 + 1;
    assert!(record_len >= REASON_OFFSET + 2);
    let reason_start = payload_start + REASON_OFFSET;
    assert_eq!(snapshot[reason_start - 1], 0x03);
    assert_eq!(
        u16::from_be_bytes(snapshot[reason_start..reason_start + 2].try_into().unwrap()),
        0x0001
    );
    snapshot[reason_start..reason_start + 2].copy_from_slice(&value.to_be_bytes());

    let record_crc_start = payload_start + record_len;
    let record_crc = crc32c(&snapshot[record_start..record_crc_start]);
    snapshot[record_crc_start..record_crc_start + 4].copy_from_slice(&record_crc.to_be_bytes());
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
    FileId::from_bytes(bytes)
}

/// A registration whose advisory path is as long as the format allows, so
/// a handful of records approach the worst case the size formulas bound.
fn widest_registration(index: u64) -> RegisterFile {
    RegisterFile {
        file_id: wide_file_id(index),
        // The widest locator variant; `register_file` must still carry the
        // clean resume state and epoch 1 that replay requires.
        locator: Locator::WindowsVolumeFileId {
            volume_serial: index,
            file_id: [0xAB; 16],
        },
        fingerprint: vec![0x5A; 16],
        advisory_path: AdvisoryPath::from_unix_bytes(&vec![b'p'; ADVISORY_PATH_STORED_MAX_BYTES])
            .unwrap(),
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

/// Scenario: a versioned namespace is opened below an engine state root that
/// does not exist.
/// Guarantees: the store refuses to create the engine-owned root and creates
/// no descendant checkpoint path.
#[test]
fn versioned_namespace_requires_an_existing_engine_state_root() {
    let dir = tempfile::tempdir().expect("temp dir");
    let state_dir = dir.path().join("missing-state");
    let options = StoreOptions::from_state_dir(&state_dir, NAMESPACE_ID).unwrap();
    let namespace = options.namespace_dir.clone();

    assert!(matches!(
        CheckpointStore::open(options).expect_err("missing state root is refused"),
        StoreError::Io { .. }
    ));
    assert!(!state_dir.exists());
    assert!(!namespace.exists());
}

/// Scenario: every parent-directory sync boundary fails once during initial
/// versioned namespace creation and once after the complete tree already
/// exists.
/// Guarantees: each failure is resumable, all three parent syncs are
/// unconditional, and a later open publishes one complete generation zero.
#[test]
fn versioned_namespace_parent_sync_faults_are_resumable_and_unconditional() {
    for point in FaultPoint::NAMESPACE_CREATION {
        let dir = tempfile::tempdir().expect("temp dir");
        let state_dir = dir.path().join("state");
        fs::create_dir(&state_dir).expect("engine state root exists");
        let mut options = StoreOptions::from_state_dir(&state_dir, NAMESPACE_ID).unwrap();
        options.ownership_timeout = Duration::from_millis(200);
        options.ownership_retry_interval = Duration::from_millis(10);

        assert!(matches!(
            CheckpointStore::open_with_fault(options.clone(), point)
                .expect_err("the parent sync boundary fails"),
            StoreError::InjectedFault { point: fired } if fired == point
        ));
        let created = CheckpointStore::open(options.clone()).expect("creation resumes");
        assert_eq!(created.generation(), 0);
        drop(created);

        assert!(matches!(
            CheckpointStore::open_with_fault(options.clone(), point)
                .expect_err("existing ancestors are still synced"),
            StoreError::InjectedFault { point: fired } if fired == point
        ));
        let reopened = CheckpointStore::open(options).expect("existing namespace reopens");
        assert_eq!(reopened.generation(), 0);
    }
}

/// Scenario: the complete versioned namespace chain is prepared, then its
/// `filelog` ancestor is replaced before publication.
/// Guarantees: retained ancestor bindings detect the replacement instead of
/// accepting descendants under a different unsynced parent entry.
#[test]
fn prepared_namespace_rejects_ancestor_replacement() {
    let dir = tempfile::tempdir().expect("temp dir");
    let state_dir = dir.path().join("state");
    fs::create_dir(&state_dir).expect("engine state root exists");
    let options = StoreOptions::from_state_dir(&state_dir, NAMESPACE_ID).unwrap();
    let mut faults = FaultPlan::disabled();
    let prepared = super::fsio::create_namespace_dir_cancellable(
        &options.namespace_dir,
        &mut faults,
        &mut || false,
    )
    .expect("namespace preparation succeeds")
    .expect("preparation is not cancelled");

    let filelog = state_dir.join("filelog");
    let displaced = state_dir.join("filelog-displaced");
    fs::rename(&filelog, &displaced).expect("the bound ancestor is displaced");
    fs::create_dir(&filelog).expect("a replacement ancestor is created");

    assert!(matches!(
        prepared
            .verify("verify the replaced namespace chain")
            .expect_err("ancestor replacement is rejected"),
        StoreError::UnsafeFilesystemObject { .. } | StoreError::Io { .. }
    ));
}

/// Scenario: first publication and compaction fail after writing their
/// snapshot temporary files.
/// Guarantees: each sequence uses only its role-specific `.create.tmp` or
/// `.compact.tmp` name and never the old generic temporary layout.
#[test]
fn publication_uses_role_specific_temporary_names() {
    let dir = tempfile::tempdir().expect("temp dir");
    let create_path = dir.path().join("create");
    assert!(
        CheckpointStore::open_with_fault(options(&create_path), FaultPoint::AfterSnapshotWrite)
            .is_err()
    );
    assert!(
        create_path
            .join(temp_file_name(
                &snapshot_file_name(0),
                PublicationRole::Create
            ))
            .is_file()
    );

    let compact_path = dir.path().join("compact");
    let mut store = open(&compact_path);
    store.faults = FaultPlan::armed(FaultPoint::AfterSnapshotWrite);
    assert!(store.compact().is_err());
    assert!(
        compact_path
            .join(temp_file_name(
                &snapshot_file_name(1),
                PublicationRole::Compact
            ))
            .is_file()
    );
}

/// Scenario: generation publication encounters an existing final name, then
/// an existing role-specific temporary name.
/// Guarantees: exclusive installation never replaces either object and
/// preserves both conflicting byte sequences for cleanup or diagnosis.
#[test]
fn generation_publication_is_exclusive_and_never_replaces_collisions() {
    use super::fsio::{AtomicInstallMode, AtomicWriteFaults};

    let dir = tempfile::tempdir().expect("temp dir");
    let final_name = "offsets-7.snapshot";
    let final_path = dir.path().join(final_name);
    write_bytes(&final_path, b"existing-final");
    let mut faults = FaultPlan::disabled();
    let error = super::fsio::write_file_atomically(
        dir.path(),
        final_name,
        b"new-snapshot",
        PublicationRole::Compact,
        AtomicInstallMode::NoReplace,
        &mut faults,
        AtomicWriteFaults::SNAPSHOT,
    )
    .expect_err("the final-name collision is refused");
    assert!(matches!(
        error.error,
        StoreError::Io { source, .. }
            if source.kind() == std::io::ErrorKind::AlreadyExists
    ));
    assert_eq!(fs::read(&final_path).unwrap(), b"existing-final");
    assert_eq!(
        fs::read(
            dir.path()
                .join(temp_file_name(final_name, PublicationRole::Compact))
        )
        .unwrap(),
        b"new-snapshot"
    );

    let second_name = "offsets-8.snapshot";
    let second_temp = dir
        .path()
        .join(temp_file_name(second_name, PublicationRole::Compact));
    write_bytes(&second_temp, b"existing-temp");
    let error = super::fsio::write_file_atomically(
        dir.path(),
        second_name,
        b"new-snapshot",
        PublicationRole::Compact,
        AtomicInstallMode::NoReplace,
        &mut faults,
        AtomicWriteFaults::SNAPSHOT,
    )
    .expect_err("the temporary-name collision is refused");
    assert!(matches!(
        error.error,
        StoreError::Io { source, .. }
            if source.kind() == std::io::ErrorKind::AlreadyExists
    ));
    assert_eq!(fs::read(second_temp).unwrap(), b"existing-temp");
    assert!(!dir.path().join(second_name).exists());
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
    let foreign = encode_snapshot(9, NAMESPACE_ID, &[]).expect("encodes");
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

    let foreign = encode_wal_header(4, NAMESPACE_ID).expect("encodes");
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

/// Scenario: foreign-namespace snapshot and WAL headers are each followed by
/// bytes that would fail later record or transaction validation.
/// Guarantees: the store rejects the namespace digest before allocating
/// snapshot state or scanning any WAL transaction.
#[test]
fn namespace_digest_is_checked_before_snapshot_or_wal_replay() {
    let snapshot_dir = tempfile::tempdir().expect("temp dir");
    let snapshot_path = snapshot_dir.path().join("snapshot-namespace");
    drop(open(&snapshot_path));
    let mut foreign_snapshot =
        encode_snapshot(0, "foreign-namespace", &[]).expect("foreign snapshot encodes");
    foreign_snapshot[52..56].copy_from_slice(&1u32.to_be_bytes());
    let header_crc = crc32c(&foreign_snapshot[..56]);
    foreign_snapshot[56..60].copy_from_slice(&header_crc.to_be_bytes());
    write_bytes(
        &snapshot_path.join(snapshot_file_name(0)),
        &foreign_snapshot,
    );

    assert!(matches!(
        CheckpointStore::open(StoreOptions {
            max_tracked_files: 8,
            ..options(&snapshot_path)
        })
        .expect_err("the foreign snapshot namespace fails first"),
        StoreError::NamespaceMismatch {
            artifact: "snapshot",
            ..
        }
    ));

    let wal_dir = tempfile::tempdir().expect("temp dir");
    let wal_path = wal_dir.path().join("wal-namespace");
    drop(open(&wal_path));
    let mut foreign_wal =
        encode_wal_header(0, "foreign-namespace").expect("foreign WAL header encodes");
    foreign_wal.extend_from_slice(&[0x00, 0x01, 0x02]);
    write_bytes(&wal_path.join(wal_file_name(0)), &foreign_wal);

    assert!(matches!(
        CheckpointStore::open(options(&wal_path))
            .expect_err("the foreign WAL namespace fails before its suffix is scanned"),
        StoreError::NamespaceMismatch {
            artifact: "WAL",
            ..
        }
    ));
    assert_eq!(
        fs::read(wal_path.join(wal_file_name(0))).expect("foreign WAL evidence remains readable"),
        foreign_wal
    );
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

    let count = usize::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX) + 5;
    let registrations: Vec<RegisterFile> = (0..count)
        .map(|index| {
            let mut register = registration(1);
            let mut bytes = [0u8; 16];
            bytes[0..8].copy_from_slice(&(index as u64).to_be_bytes());
            register.file_id = FileId::from_bytes(bytes);
            register.locator = Locator::PosixDevIno {
                dev: 7,
                ino: index as u64 + 1,
            };
            register
        })
        .collect();

    let outcomes = store.register_files(registrations).expect("registers");
    assert_eq!(outcomes.len(), 2);
    assert_eq!(
        outcomes[0].operations,
        usize::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX)
    );
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
/// Guarantees: the first chunk remains durable, the definitive no-write
/// leaves the live handle usable, and retrying the absent suffix completes
/// the logical batch without duplication.
#[test]
fn filesystem_failure_between_batch_chunks_recovers_a_retryable_prefix() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let count = usize::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX) + 1;
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
    store
        .sync()
        .expect("the completed first chunk remains syncable");
    assert_eq!(
        store.table().len(),
        usize::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX)
    );
    assert!(store.table().get(&retry.file_id).is_none());

    let outcomes = store
        .register_files(vec![retry.clone()])
        .expect("the absent suffix retries");
    assert_eq!(outcomes[0].sequence, 2);
    assert_eq!(store.table().len(), count);
    drop(store);

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
    let count = usize::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX) + 1;

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
        max_tracked_files: WAL_MAX_NON_PROGRESS_OPS_PER_TX.into(),
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
        } if registrations == count && max == u32::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX)
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
    conflicting.advisory_path = AdvisoryPath::from_unix_bytes(b"/var/log/conflicting.log").unwrap();
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

/// Scenario: a registration batch spans three transaction-count-limited WAL
/// generations.
/// Guarantees: deterministic preflight models sequence resets, each later
/// chunk compacts before append, the second compaction first cleans the prior
/// retired generation, and every registration becomes durable.
#[test]
fn batched_preflight_models_multiple_preappend_compactions() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let count = usize::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX) * 2 + 1;
    let mut store = CheckpointStore::open(StoreOptions {
        compact_after_transactions: 1,
        fingerprint_bytes: 16,
        max_tracked_files: count as u32,
        ..options(&path)
    })
    .expect("namespace opens");
    let registrations: Vec<RegisterFile> = (0..count as u64).map(widest_registration).collect();

    let outcomes = store.register_files(registrations).unwrap();
    assert_eq!(outcomes.len(), 3);
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| outcome.sequence)
            .collect::<Vec<_>>(),
        vec![1, 1, 1]
    );
    assert_eq!(store.table().len(), count);
    assert_eq!(store.generation(), 2);
    assert_eq!(store.stats().wal_transactions, 1);
    assert_eq!(store.stats().preappend_compactions, 2);
    assert_eq!(store.stats().preappend_cleanup_generations, 1);
    assert_eq!(store.retired_generations(), [1]);
    drop(store);

    let reopened = CheckpointStore::open(StoreOptions {
        compact_after_transactions: 1,
        fingerprint_bytes: 16,
        max_tracked_files: count as u32,
        ..options(&path)
    })
    .unwrap();
    assert_eq!(reopened.generation(), 2);
    assert_eq!(reopened.table().len(), count);
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

/// Scenario: an Ack appends offset and continuation progress under a deferred
/// sync interval, and a crash leaves the prior WAL, a torn append prefix, or
/// the complete append.
/// Guarantees: recovery yields the complete old or Acked framing tuple, never
/// a partial transaction, mixed offset/resume state, or progress beyond the Ack.
#[test]
fn deferred_ack_crash_images_recover_old_or_complete_progress() {
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
    let wal_path = path.join(wal_file_name(store.generation()));
    let prior_durable_image = fs::read(&wal_path).expect("prior WAL reads");
    let outcomes = store
        .commit_progress(vec![UpdateProgress {
            new_framing_resume: FramingResume::Continuation {
                record_start_offset: 96,
                record_end_offset: 0,
                next_fragment_index: 2,
            },
            ..progress(1, 0, 128)
        }])
        .expect("Ack progress appends");
    assert!(!outcomes[0].synced);
    let complete_append_image = fs::read(&wal_path).expect("appended WAL reads");
    drop(store);

    write_bytes(&wal_path, &prior_durable_image);
    let prior = open(&path);
    let prior_record = prior.table().get(&file_id(1)).expect("record is tracked");
    assert_eq!(
        (prior_record.committed_offset, prior_record.framing_resume),
        (0, FramingResume::Clean)
    );
    drop(prior);

    let torn_len =
        prior_durable_image.len() + (complete_append_image.len() - prior_durable_image.len()) / 2;
    assert!(torn_len > prior_durable_image.len());
    assert!(torn_len < complete_append_image.len());
    write_bytes(&wal_path, &complete_append_image[..torn_len]);
    let torn = open(&path);
    let torn_record = torn.table().get(&file_id(1)).expect("record is tracked");
    assert_eq!(
        (torn_record.committed_offset, torn_record.framing_resume),
        (0, FramingResume::Clean)
    );
    assert_eq!(
        torn.recovery().torn_tail_bytes,
        torn_len - prior_durable_image.len()
    );
    drop(torn);

    write_bytes(&wal_path, &complete_append_image);
    let complete = open(&path);
    let complete_record = complete
        .table()
        .get(&file_id(1))
        .expect("record is tracked");
    assert_eq!(
        (
            complete_record.committed_offset,
            complete_record.framing_resume
        ),
        (
            128,
            FramingResume::Continuation {
                record_start_offset: 96,
                record_end_offset: 0,
                next_fragment_index: 2,
            }
        )
    );
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
            new_fingerprint: vec![9; 8],
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
            new_committed_frontier_guard: zero_guard(0),
            new_framing_resume: FramingResume::Clean,
            new_fingerprint: vec![2; 8],
            action_time_unix_nano: 5_000,
            namespace_id: NAMESPACE_ID.to_owned(),
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
        let new_fingerprint = if remains_quarantined {
            vec![1; 8]
        } else {
            vec![9; 8]
        };

        let outcome = store
            .reset_quarantined_file(ResetQuarantinedFile {
                file_id: file_id(1),
                expected_quarantine_epoch: 1,
                action,
                resulting_epoch,
                resulting_offset,
                new_committed_frontier_guard: zero_guard(resulting_offset),
                new_framing_resume: FramingResume::Clean,
                new_fingerprint: new_fingerprint.clone(),
                action_time_unix_nano: 5_000,
                namespace_id: NAMESPACE_ID.to_owned(),
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
        assert_eq!(record.fingerprint, new_fingerprint);

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
        assert_eq!(record.fingerprint, new_fingerprint);
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
            new_committed_frontier_guard: zero_guard(0),
            new_framing_resume: FramingResume::Clean,
            new_fingerprint: vec![1; 8],
            action_time_unix_nano: 5_000,
            namespace_id: NAMESPACE_ID.to_owned(),
            audit_reason: String::new(),
        })
        .expect_err("an empty audit reason is refused");
    assert!(matches!(error, StoreError::AuditReasonRequired { .. }));
    assert_eq!(store.stats().next_sequence, sequence_before);
}

/// Scenario: a regular WAL read reaches physical EOF with a final fragment
/// too short to declare a transaction frame.
/// Guarantees: only after that EOF proof does the store classify the codec's
/// `Incomplete` suffix as torn, truncate exactly those bytes, replay every
/// complete transaction, and continue with the next sequence.
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

/// Scenario: each ordinary WAL write and sync boundary fails once while an
/// immediately durable registration is appended, then the exact transaction
/// is retried.
/// Guarantees: no-write retries append once, partial writes are repaired,
/// complete writes are never appended again, required sync is retried, and
/// the transaction is applied exactly once.
#[test]
fn wal_append_faults_reconcile_the_exact_retry_once() {
    for point in FaultPoint::WAL_DURABILITY {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("namespace");
        drop(open(&path));
        let wal_path = path.join(wal_file_name(0));
        let initial_wal = fs::read(&wal_path).expect("initial WAL reads");

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
        assert!(store.table().is_empty());
        let known_complete_waiting_sync =
            matches!(point, FaultPoint::BeforeWalSync | FaultPoint::AfterWalSync);
        assert_eq!(
            store.stats().wal_transactions,
            u64::from(known_complete_waiting_sync)
        );
        assert_eq!(store.stats().next_sequence, 1);
        let failed_wal = fs::read(&wal_path).expect("failed WAL reads");
        let transaction_was_written = !matches!(
            point,
            FaultPoint::BeforeWalTransactionWrite | FaultPoint::DuringWalTransactionWrite
        );
        if point == FaultPoint::BeforeWalTransactionWrite {
            assert_eq!(failed_wal, initial_wal);
            store
                .sync()
                .expect("a definitive no-write leaves no repair");
        } else {
            assert!(failed_wal.len() > initial_wal.len());
            assert!(matches!(
                store
                    .sync()
                    .expect_err("unrelated operations wait for exact append repair"),
                StoreError::PendingWalAppend { sequence: 1, .. }
            ));
            assert!(matches!(
                store
                    .register_files(vec![registration(2)])
                    .expect_err("a different transaction cannot consume the pending sequence"),
                StoreError::PendingWalAppendMismatch {
                    expected_sequence: 1,
                    found_sequence: 1,
                    ..
                }
            ));
        }

        let outcomes = store
            .register_files(vec![registration(1)])
            .expect("the exact bounded retry succeeds");
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].sequence, 1);
        assert!(outcomes[0].synced);
        assert_eq!(store.table().len(), 1);
        assert!(store.table().get(&file_id(1)).is_some());
        assert_eq!(store.stats().wal_transactions, 1);
        assert_eq!(store.stats().next_sequence, 2);
        let repaired_wal = fs::read(&wal_path).expect("repaired WAL reads");
        if transaction_was_written {
            assert_eq!(
                repaired_wal, failed_wal,
                "a complete append at {point} must not be written twice"
            );
        } else {
            assert!(repaired_wal.len() > initial_wal.len());
        }

        drop(store);
        let reopened = open(&path);
        assert_eq!(reopened.recovery().transactions_replayed, 1);
        assert_eq!(reopened.recovery().torn_tail_bytes, 0);
        assert_eq!(reopened.table().len(), 1);
        assert!(reopened.table().get(&file_id(1)).is_some());
    }
}

/// Scenario: an ambiguous append leaves one complete transaction whose frame
/// CRC is corrupted before the exact live retry.
/// Guarantees: reconciliation fails closed, preserves every byte for
/// evidence, and never truncates or applies a structurally complete invalid
/// frame.
#[test]
fn wal_append_retry_rejects_complete_corruption_without_truncation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    drop(open(&path));
    let wal_path = path.join(wal_file_name(0));

    let mut store = CheckpointStore::open_with_fault(
        StoreOptions {
            sync_interval: Duration::ZERO,
            ..options(&path)
        },
        FaultPoint::AfterWalTransactionWrite,
    )
    .expect("existing namespace opens without appending");
    let error = store
        .register_files(vec![registration(1)])
        .expect_err("the ambiguous append boundary fails");
    assert!(matches!(
        error,
        StoreError::InjectedFault {
            point: FaultPoint::AfterWalTransactionWrite
        }
    ));

    let mut corrupted = fs::read(&wal_path).expect("complete WAL reads");
    let final_byte = corrupted.last_mut().expect("transaction has a frame CRC");
    *final_byte ^= 0x01;
    write_bytes(&wal_path, &corrupted);

    let error = store
        .register_files(vec![registration(1)])
        .expect_err("complete corruption fails reconciliation");
    assert!(matches!(error, StoreError::Decode { .. }));
    assert_eq!(
        fs::read(&wal_path).expect("corrupt WAL remains readable"),
        corrupted
    );
    assert!(store.table().is_empty());
    assert!(matches!(
        store
            .sync()
            .expect_err("the pending append still blocks unrelated sync"),
        StoreError::PendingWalAppend { sequence: 1, .. }
    ));
    drop(store);

    assert!(matches!(
        CheckpointStore::open(options(&path)).expect_err("reopen fails closed"),
        StoreError::Decode { .. }
    ));
    assert_eq!(
        fs::read(&wal_path).expect("corrupt WAL evidence remains"),
        corrupted
    );
}

/// Scenario: the store retains an uncertain maximum-sized transaction only
/// by fixed accounting and digest fields.
/// Guarantees: append-retry state cannot grow with transaction payload size.
#[test]
fn pending_wal_append_state_is_fixed_size() {
    assert!(size_of::<super::PendingWalAppend>() <= 128);
}

/// Scenario: live repair truncates a known partial append, but a fault makes
/// the post-sync outcome uncertain before the exact transaction is retried.
/// Guarantees: the shortened WAL retains a required-repair-sync obligation;
/// the next retry syncs that boundary again before appending one transaction.
#[test]
fn torn_append_repair_retries_sync_before_reappend() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    drop(open(&path));
    let wal_path = path.join(wal_file_name(0));
    let boundary = fs::metadata(&wal_path).expect("initial WAL metadata").len();

    let mut store = CheckpointStore::open_with_fault(
        StoreOptions {
            sync_interval: Duration::ZERO,
            ..options(&path)
        },
        FaultPoint::DuringWalTransactionWrite,
    )
    .expect("existing namespace opens without appending");
    assert!(matches!(
        store
            .register_files(vec![registration(1)])
            .expect_err("the partial append fault fires"),
        StoreError::InjectedFault {
            point: FaultPoint::DuringWalTransactionWrite
        }
    ));
    store.faults = FaultPlan::armed(FaultPoint::AfterTornTailTruncate);
    assert!(matches!(
        store
            .register_files(vec![registration(1)])
            .expect_err("repair sync outcome is uncertain"),
        StoreError::InjectedFault {
            point: FaultPoint::AfterTornTailTruncate
        }
    ));
    assert_eq!(
        fs::metadata(&wal_path)
            .expect("truncated WAL metadata")
            .len(),
        boundary
    );
    assert!(
        store
            .pending_wal_append
            .expect("the append remains pending")
            .repair_sync_required
    );

    let outcomes = store
        .register_files(vec![registration(1)])
        .expect("the repair sync and exact append retry succeed");
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].sequence, 1);
    assert!(outcomes[0].synced);
    assert_eq!(store.table().len(), 1);
    assert_eq!(store.stats().wal_transactions, 1);
    drop(store);

    let reopened = open(&path);
    assert_eq!(reopened.recovery().transactions_replayed, 1);
    assert!(reopened.table().get(&file_id(1)).is_some());
}

/// Scenario: a complete uncertain append is validated while the WAL pathname
/// is replaced with another regular file before the exact retry.
/// Guarantees: stable file identity rejects the replacement before sync,
/// truncation, handle installation, or logical application.
#[test]
fn wal_append_reconciliation_rejects_regular_file_replacement() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    drop(open(&path));
    let wal_path = path.join(wal_file_name(0));
    let displaced = path.join("displaced.wal");

    let mut store = CheckpointStore::open_with_fault(
        StoreOptions {
            sync_interval: Duration::ZERO,
            ..options(&path)
        },
        FaultPoint::AfterWalTransactionWrite,
    )
    .expect("existing namespace opens without appending");
    assert!(store.register_files(vec![registration(1)]).is_err());
    fs::rename(&wal_path, &displaced).expect("the original WAL is displaced");
    let replacement = encode_wal_header(0, NAMESPACE_ID).expect("replacement WAL encodes");
    write_bytes(&wal_path, &replacement);

    assert!(matches!(
        store
            .register_files(vec![registration(1)])
            .expect_err("the replacement is rejected"),
        StoreError::UnsafeFilesystemObject { .. }
    ));
    assert_eq!(
        fs::read(&wal_path).expect("replacement WAL remains readable"),
        replacement
    );
    assert!(store.table().is_empty());
}

/// Scenario: an administrative removal's complete append has an uncertain
/// result, then the exact convenience API call is retried.
/// Guarantees: the pre-append gate permits the exact removal to reach WAL
/// reconciliation and removes the quarantined record with one transaction.
#[test]
fn quarantined_removal_retries_the_pending_exact_append() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut seed = open(&path);
    let _registered = seed
        .register_files(vec![registration(1)])
        .expect("registration succeeds");
    let _quarantined = seed
        .quarantine_files(vec![quarantine(1)])
        .expect("quarantine succeeds");
    drop(seed);

    let mut store =
        CheckpointStore::open_with_fault(options(&path), FaultPoint::AfterWalTransactionWrite)
            .expect("the quarantined namespace opens");
    assert!(matches!(
        store
            .remove_quarantined_file(file_id(1), 0x0008, 10, "operator purge".to_owned())
            .expect_err("the first removal result is uncertain"),
        StoreError::InjectedFault {
            point: FaultPoint::AfterWalTransactionWrite
        }
    ));
    let outcome = store
        .remove_quarantined_file(file_id(1), 0x0008, 10, "operator purge".to_owned())
        .expect("the exact removal retry reconciles")
        .expect("the record was present before the pending transaction");
    assert_eq!(outcome.sequence, 3);
    assert!(outcome.synced);
    assert!(store.table().is_empty());
    assert_eq!(store.stats().wal_transactions, 3);
}

/// Scenario: a complete append is uncertain in generation zero, then a
/// test-only threshold reduction would otherwise require compaction before
/// its exact retry.
/// Guarantees: pending transaction reconciliation takes precedence over
/// compaction and commits the original sequence in the original generation.
#[test]
fn pending_append_is_reconciled_before_compaction_decision() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let configured = || StoreOptions {
        compact_after_transactions: 2,
        ..options(&path)
    };
    let mut seeded = CheckpointStore::open(configured()).unwrap();
    let _registered = seeded.register_files(vec![registration(1)]).unwrap();
    drop(seeded);

    let mut store =
        CheckpointStore::open_with_fault(configured(), FaultPoint::AfterWalTransactionWrite)
            .unwrap();
    assert!(matches!(
        store.register_files(vec![registration(2)]).unwrap_err(),
        StoreError::InjectedFault {
            point: FaultPoint::AfterWalTransactionWrite
        }
    ));
    assert!(store.has_pending_wal_append());
    store.compact_after_transactions = 1;

    let outcomes = store.register_files(vec![registration(2)]).unwrap();
    assert_eq!(outcomes[0].sequence, 2);
    assert_eq!(store.generation(), 0);
    assert_eq!(store.stats().preappend_compactions, 0);
    assert_eq!(store.table().len(), 2);
}

/// Scenario: a second required pre-append compaction first encounters a
/// retired-generation cleanup fault.
/// Guarantees: the WAL stays at its threshold without appending, cleanup
/// failure is counted, and an exact retry cleans, compacts, and appends once.
#[test]
fn preappend_compaction_waits_for_retired_cleanup() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let configured = || StoreOptions {
        compact_after_transactions: 1,
        ..options(&path)
    };
    let mut store = CheckpointStore::open(configured()).unwrap();
    let _first = store.register_files(vec![registration(1)]).unwrap();
    let _second = store.register_files(vec![registration(2)]).unwrap();
    assert_eq!(store.generation(), 1);
    assert_eq!(store.retired_generations(), [0]);
    assert_eq!(store.stats().wal_transactions, 1);

    store.faults = FaultPlan::armed(FaultPoint::BeforeRetiredGenerationRemoval);
    let error = store.register_files(vec![registration(3)]).unwrap_err();
    assert!(matches!(
        error,
        StoreError::InjectedFault {
            point: FaultPoint::BeforeRetiredGenerationRemoval
        }
    ));
    assert_eq!(store.generation(), 1);
    assert_eq!(store.stats().wal_transactions, 1);
    assert_eq!(store.stats().preappend_cleanup_failures, 1);
    assert_eq!(store.table().len(), 2);

    let outcomes = store.register_files(vec![registration(3)]).unwrap();
    assert_eq!(outcomes[0].sequence, 1);
    assert_eq!(store.generation(), 2);
    assert_eq!(store.stats().wal_transactions, 1);
    assert_eq!(store.stats().preappend_compactions, 2);
    assert_eq!(store.stats().preappend_cleanup_generations, 1);
    assert_eq!(store.table().len(), 3);
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

/// Scenario: the final byte of a structurally complete WAL transaction's
/// frame CRC is altered after the transaction was written.
/// Guarantees: complete final-frame corruption fails closed, is never
/// classified as `Incomplete`, and remains byte-for-byte intact for evidence.
#[test]
fn corrupted_wal_transaction_fails_recovery_closed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let _seeded = seeded_namespace(&path);

    let wal_path = path.join(wal_file_name(0));
    let mut bytes = fs::read(&wal_path).expect("wal reads");
    *bytes.last_mut().expect("the WAL has a final frame CRC") ^= 0xFF;
    write_bytes(&wal_path, &bytes);

    let error = CheckpointStore::open(options(&path)).expect_err("corruption fails closed");
    match error {
        StoreError::Decode { artifact, .. } => assert_eq!(artifact, "WAL"),
        other => panic!("expected a WAL decode failure, got {other:?}"),
    }
    assert_eq!(
        fs::read(&wal_path).expect("corrupt WAL evidence remains readable"),
        bytes
    );
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

/// Scenario: cancellation becomes visible after cleanup removes the retired
/// WAL but before it removes the retired snapshot.
/// Guarantees: cleanup starts no later unlink, retains the complete retired
/// list, and an uncancelled retry resumes idempotently.
#[test]
fn retired_generation_cleanup_cancellation_stops_between_unlinks() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = open(&path);
    store.compact().expect("compaction succeeds");
    let retired_snapshot = path.join(snapshot_file_name(0));
    let retired_wal = path.join(wal_file_name(0));

    let outcome = store
        .cleanup_retired_generations_cancellable(|| !retired_wal.exists())
        .expect("cancellation is not a cleanup error");

    assert!(outcome.is_none());
    assert!(!retired_wal.exists());
    assert!(retired_snapshot.is_file());
    assert_eq!(store.retired_generations(), [0]);
    assert_eq!(
        store
            .cleanup_retired_generations()
            .expect("cleanup retry succeeds"),
        1
    );
    assert!(!retired_snapshot.exists());
    assert!(store.retired_generations().is_empty());
}

/// Scenario: a retired snapshot pathname is replaced by either a symlink or a
/// second hard link before cleanup begins.
/// Guarantees: cleanup validates and binds the complete retired pair before
/// deleting its WAL, rejects both substitutions, and preserves every target.
#[cfg(unix)]
#[test]
fn retired_cleanup_rejects_snapshot_substitution_before_wal_removal() {
    use std::os::unix::fs::symlink;

    for hard_link in [false, true] {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("namespace");
        let mut store = open(&path);
        store.compact().expect("compaction succeeds");
        let retired_snapshot = path.join(snapshot_file_name(0));
        let retired_wal = path.join(wal_file_name(0));
        let displaced = path.join(if hard_link {
            "retired.snapshot.hardlink-target"
        } else {
            "retired.snapshot.symlink-target"
        });
        fs::rename(&retired_snapshot, &displaced).unwrap();
        if hard_link {
            fs::hard_link(&displaced, &retired_snapshot).unwrap();
        } else {
            symlink(&displaced, &retired_snapshot).unwrap();
        }
        let target_bytes = fs::read(&displaced).unwrap();

        assert!(
            store.cleanup_retired_generations().is_err(),
            "cleanup accepted {} substitution",
            if hard_link { "hard-link" } else { "symlink" }
        );
        assert!(retired_wal.is_file());
        assert_eq!(fs::read(&displaced).unwrap(), target_bytes);
        assert_eq!(store.retired_generations(), [0]);
    }
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
            !matches!(
                point,
                FaultPoint::BeforeSnapshotWrite
                    | FaultPoint::AfterMarkerPublish
                    | FaultPoint::BeforeMarkerDirSync
                    | FaultPoint::AfterMarkerDirSync
            ),
            "unexpected temporary-file cleanup after a fault at {point}"
        );
    }
}

/// Scenario: compaction fails before marker publication after installing
/// both proposed generation files, then retries on the same store handle.
/// Guarantees: the retry first removes and syncs the exact abandoned proposal,
/// reuses the unpublished number, and publishes one complete new generation.
#[test]
fn compaction_retry_cleans_the_exact_abandoned_proposal() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let seeded = seeded_namespace(&path);
    let mut store =
        CheckpointStore::open_with_fault(options(&path), FaultPoint::BeforeMarkerWrite).unwrap();

    assert!(matches!(
        store
            .compact()
            .expect_err("the first marker write is faulted"),
        StoreError::InjectedFault {
            point: FaultPoint::BeforeMarkerWrite
        }
    ));
    assert!(path.join(snapshot_file_name(1)).is_file());
    assert!(path.join(wal_file_name(1)).is_file());

    store
        .compact()
        .expect("the exact abandoned proposal is retried");
    assert_eq!(store.generation(), 1);
    assert_eq!(records(&store), seeded);
    assert!(
        !path
            .join(temp_file_name(
                &snapshot_file_name(1),
                PublicationRole::Compact
            ))
            .exists()
    );
    assert!(
        !path
            .join(temp_file_name(&wal_file_name(1), PublicationRole::Compact))
            .exists()
    );
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

/// Scenario: `CURRENT` is absent while the namespace contains only the exact
/// first-publication temporary/final names and the ownership lock.
/// Guarantees: recovery removes that bounded set, syncs the namespace, and
/// republishes a fresh empty generation zero instead of adopting any bytes.
#[test]
fn markerless_exact_initial_publication_is_cleaned_and_restarted() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let _seeded = seeded_namespace(&path);
    fs::remove_file(path.join(CURRENT_FILE_NAME)).expect("removes CURRENT");
    write_bytes(
        &path.join(CURRENT_CREATE_TEMP_FILE_NAME),
        b"untrusted marker bytes",
    );
    write_bytes(
        &path.join(temp_file_name(
            &snapshot_file_name(0),
            PublicationRole::Create,
        )),
        b"partial snapshot",
    );

    let reopened = open(&path);
    assert!(reopened.recovery().created);
    assert_eq!(reopened.generation(), 0);
    assert!(reopened.table().is_empty());
    assert!(path.join(CURRENT_FILE_NAME).is_file());
    assert!(!path.join(CURRENT_CREATE_TEMP_FILE_NAME).exists());
    assert!(
        !path
            .join(temp_file_name(
                &snapshot_file_name(0),
                PublicationRole::Create
            ))
            .exists()
    );
}

/// Scenario: `CURRENT` is absent and the directory also contains either an
/// unrelated name or a later-generation artifact.
/// Guarantees: recovery preserves every byte and reports ambiguous authority
/// instead of adopting, deleting, or recreating the namespace.
#[test]
fn markerless_unrecognized_artifacts_fail_closed_without_cleanup() {
    for extra_name in ["notes.txt".to_owned(), snapshot_file_name(1)] {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("namespace");
        drop(open(&path));
        fs::remove_file(path.join(CURRENT_FILE_NAME)).expect("removes CURRENT");
        write_bytes(&path.join(&extra_name), b"evidence");
        let before = fs::read_dir(&path)
            .expect("namespace lists")
            .map(|entry| entry.expect("entry reads").file_name())
            .collect::<BTreeSet<_>>();

        assert!(matches!(
            CheckpointStore::open(options(&path)).expect_err("authority is ambiguous"),
            StoreError::AuthorityMissingOrAmbiguous { .. }
        ));
        let after = fs::read_dir(&path)
            .expect("namespace lists")
            .map(|entry| entry.expect("entry reads").file_name())
            .collect::<BTreeSet<_>>();
        assert_eq!(after, before);
    }
}

/// Scenario: valid `CURRENT` selects generation one while exact stale create
/// and next-generation compact temporaries remain.
/// Guarantees: recovery validates current authority first, removes only those
/// recognized abandoned publication artifacts, and preserves all other files.
#[test]
fn valid_current_cleans_only_exact_abandoned_publication_artifacts() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let seeded = seeded_namespace(&path);
    let mut store = open(&path);
    store.compact().expect("compaction succeeds");
    drop(store);
    for name in [
        CURRENT_CREATE_TEMP_FILE_NAME.to_owned(),
        CURRENT_COMPACT_TEMP_FILE_NAME.to_owned(),
        temp_file_name(&snapshot_file_name(0), PublicationRole::Create),
        temp_file_name(&wal_file_name(0), PublicationRole::Create),
        snapshot_file_name(2),
        wal_file_name(2),
        temp_file_name(&snapshot_file_name(2), PublicationRole::Compact),
        temp_file_name(&wal_file_name(2), PublicationRole::Compact),
    ] {
        write_bytes(&path.join(name), b"stale");
    }
    write_bytes(&path.join("notes.tmp"), b"unrelated");

    let reopened = open(&path);
    assert_eq!(reopened.generation(), 1);
    assert_eq!(records(&reopened), seeded);
    assert_eq!(reopened.recovery().removed_temp_files, 8);
    assert!(path.join("notes.tmp").is_file());
}

/// Scenario: a valid namespace contains a case-only alias of the proposed
/// generation snapshot name.
/// Guarantees: cleanup rejects the alias before deleting anything, preventing
/// canonical-path deletion on case-insensitive filesystems.
#[test]
fn publication_cleanup_rejects_noncanonical_case_aliases() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    drop(open(&path));
    let alias = path.join("OFFSETS-1.SNAPSHOT");
    write_bytes(&alias, b"case alias");

    assert!(matches!(
        CheckpointStore::open(options(&path)).expect_err("case alias is rejected"),
        StoreError::UnsafeFilesystemObject { .. }
    ));
    assert_eq!(fs::read(alias).expect("alias survives"), b"case alias");
    assert!(path.join(CURRENT_FILE_NAME).is_file());
}

/// Scenario: `CURRENT` is corrupt while an exact compact marker temporary
/// remains.
/// Guarantees: authoritative corruption fails closed before cleanup and the
/// temporary evidence remains byte-identical.
#[test]
fn corrupt_current_preserves_publication_temporary_evidence() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    drop(open(&path));
    write_bytes(
        &path.join(CURRENT_COMPACT_TEMP_FILE_NAME),
        b"temporary evidence",
    );
    let marker_path = path.join(CURRENT_FILE_NAME);
    let mut marker = fs::read(&marker_path).expect("CURRENT reads");
    marker[0] ^= 0xff;
    write_bytes(&marker_path, &marker);

    assert!(matches!(
        CheckpointStore::open(options(&path)).expect_err("corrupt CURRENT fails closed"),
        StoreError::Decode {
            artifact: "CURRENT marker",
            ..
        }
    ));
    assert_eq!(
        fs::read(path.join(CURRENT_COMPACT_TEMP_FILE_NAME)).expect("temp reads"),
        b"temporary evidence"
    );
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

    let eligible_absent = HashSet::from([file_id(1), file_id(2), file_id(3)]);
    let error = store
        .remove_vetted_retention_records(&eligible_absent)
        .unwrap_err();
    assert!(matches!(
        error,
        StoreError::RetentionCandidateQuarantined {
            file_id: quarantined
        } if quarantined == file_id(2)
    ));
    let vetted = HashSet::from([file_id(1), file_id(3)]);
    let removed = store
        .remove_vetted_retention_records(&vetted)
        .expect("retention removal succeeds");
    assert_eq!(removed, 2);
    assert_eq!(store.generation(), 1);
    assert_eq!(store.stats().wal_transactions, 0);
    assert_eq!(
        fs::metadata(path.join(wal_file_name(1))).unwrap().len(),
        WAL_HEADER_LEN as u64
    );
    assert!(store.table().get(&file_id(1)).is_none());
    assert!(store.table().get(&file_id(3)).is_none());
    let quarantined = store.table().get(&file_id(2)).expect("quarantine survives");
    assert_eq!(quarantined.lifecycle_state, LifecycleState::Quarantined);

    let outcome = store
        .remove_quarantined_file(
            file_id(2),
            0x0008,
            10_000_000_000,
            "operator purge".to_owned(),
        )
        .expect("administrative removal succeeds")
        .expect("the record was present");
    assert!(outcome.synced);
    assert!(store.table().get(&file_id(2)).is_none());
    drop(store);

    let reopened = open(&path);
    assert_eq!(reopened.table().len(), 0);
}

/// Scenario: a vetted retention set contains more records than one
/// non-progress WAL transaction can carry.
/// Guarantees: one filtered compaction removes the complete set atomically,
/// publishes a fresh empty WAL, and never emits partial `remove_file` chunks.
#[test]
fn retention_large_set_uses_one_filtered_compaction() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let count = usize::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX) + 7;
    let registrations: Vec<RegisterFile> = (0..count as u64).map(widest_registration).collect();
    let eligible: HashSet<FileId> = registrations
        .iter()
        .map(|registration| registration.file_id)
        .collect();
    let mut store = open(&path);
    let _outcomes = store.register_files(registrations).unwrap();

    let removed = store.remove_vetted_retention_records(&eligible).unwrap();
    assert_eq!(removed, count);
    assert!(store.table().is_empty());
    assert_eq!(store.generation(), 1);
    assert_eq!(store.stats().wal_transactions, 0);
    assert_eq!(
        fs::metadata(path.join(wal_file_name(1))).unwrap().len(),
        WAL_HEADER_LEN as u64
    );
    drop(store);

    let reopened = open(&path);
    assert!(reopened.table().is_empty());
    assert_eq!(reopened.generation(), 1);
}

/// Scenario: filtered retention compaction fails at every snapshot, WAL,
/// marker, and directory-sync publication boundary.
/// Guarantees: restart selects either the complete original table or the
/// complete filtered table according to `CURRENT`, never a partially removed
/// set.
#[test]
fn retention_filtered_compaction_recovers_all_or_none_after_faults() {
    for point in FaultPoint::PUBLICATION {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("namespace");
        let mut seeded = open(&path);
        let _registered = seeded
            .register_files(vec![registration(1), registration(2)])
            .unwrap();
        drop(seeded);

        let mut store =
            CheckpointStore::open_with_fault(options(&path), point).expect("namespace reopens");
        let error = store
            .remove_vetted_retention_records(&HashSet::from([file_id(1)]))
            .unwrap_err();
        assert!(matches!(
            error,
            StoreError::InjectedFault { point: fired } if fired == point
        ));
        drop(store);

        let marker_replaced = matches!(
            point,
            FaultPoint::AfterMarkerPublish
                | FaultPoint::BeforeMarkerDirSync
                | FaultPoint::AfterMarkerDirSync
        );
        let reopened = open(&path);
        assert_eq!(reopened.generation(), u64::from(marker_replaced));
        assert_eq!(reopened.table().len(), if marker_replaced { 1 } else { 2 });
        assert_eq!(reopened.table().get(&file_id(1)).is_none(), marker_replaced);
        assert!(reopened.table().get(&file_id(2)).is_some());
    }
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

/// Scenario: valid generation zero has every exact stale create temporary and
/// proposed-generation compact temporary alongside unrelated files.
/// Guarantees: recovery removes only the role-correct abandoned names; foreign
/// `.tmp` files, authority, and the ownership lock survive.
#[test]
fn temporary_file_cleanup_removes_only_this_namespace_temporaries() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    drop(open(&path));

    let owned = [
        CURRENT_CREATE_TEMP_FILE_NAME.to_owned(),
        CURRENT_COMPACT_TEMP_FILE_NAME.to_owned(),
        temp_file_name(&snapshot_file_name(0), PublicationRole::Create),
        temp_file_name(&wal_file_name(0), PublicationRole::Create),
        temp_file_name(&snapshot_file_name(1), PublicationRole::Compact),
        temp_file_name(&wal_file_name(1), PublicationRole::Compact),
    ];
    let foreign = [
        "unrelated.tmp".to_owned(),
        "offsets-x.snapshot.compact.tmp".to_owned(),
        "offsets-03.wal.compact.tmp".to_owned(),
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

/// Scenario: a namespace contains one more recognized role-specific
/// temporary artifact than bounded recovery permits.
/// Guarantees: opening fails before deleting any candidate, so an
/// adversarial directory cannot turn one open into unbounded cleanup work
/// or make repeated opens erase an unbounded population in chunks.
#[test]
fn excessive_temporary_file_population_is_rejected_without_cleanup() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    drop(open(&path));

    let mut names = vec![
        CURRENT_CREATE_TEMP_FILE_NAME.to_owned(),
        CURRENT_COMPACT_TEMP_FILE_NAME.to_owned(),
    ];
    for generation in 0..MAX_GENERATIONS_ON_DISK as u64 + 1 {
        for final_name in [snapshot_file_name(generation), wal_file_name(generation)] {
            names.push(temp_file_name(&final_name, PublicationRole::Compact));
        }
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

/// Scenario: valid generation zero is accompanied by two unpublished future
/// generations, then by a population above the hard inventory bound.
/// Guarantees: more than the single next proposal is ambiguous and preserved;
/// an over-bound population fails before any cleanup.
#[test]
fn excessive_generation_population_is_rejected() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    drop(open(&path));
    for generation in 1..MAX_GENERATIONS_ON_DISK as u64 {
        write_bytes(&path.join(snapshot_file_name(generation)), b"not selected");
    }
    write_bytes(&path.join("unrelated.snapshot"), b"ignored");

    assert!(matches!(
        CheckpointStore::open(options(&path)).expect_err("future authority is ambiguous"),
        StoreError::AuthorityMissingOrAmbiguous { .. }
    ));
    assert!(path.join(snapshot_file_name(1)).is_file());
    assert!(path.join(snapshot_file_name(2)).is_file());

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

const NAMESPACE_LOCK_CHILD_MODE_ENV: &str = "OTAP_FILELOG_NAMESPACE_LOCK_CHILD_MODE";
const NAMESPACE_LOCK_CHILD_PATH_ENV: &str = "OTAP_FILELOG_NAMESPACE_LOCK_CHILD_PATH";
const NAMESPACE_LOCK_CHILD_TEST_NAME: &str = "receivers::filelog_receiver::checkpoint::store::\
                                              tests::namespace_lock_subprocess_helper";

fn run_namespace_lock_child(path: &Path, mode: &str) -> std::process::ExitStatus {
    std::process::Command::new(std::env::current_exe().expect("test binary path"))
        .args([
            "--ignored",
            "--exact",
            NAMESPACE_LOCK_CHILD_TEST_NAME,
            "--nocapture",
        ])
        .env(NAMESPACE_LOCK_CHILD_MODE_ENV, mode)
        .env(NAMESPACE_LOCK_CHILD_PATH_ENV, path)
        .status()
        .expect("namespace-lock child process starts")
}

/// Scenario: the current test binary is launched as a dedicated child process
/// to contend for, acquire, or terminate while holding one namespace lock.
/// Guarantees: parent tests exercise real process-scoped kernel cleanup rather
/// than simulating cross-process behavior with threads.
#[test]
#[ignore = "subprocess helper"]
#[allow(clippy::exit)]
fn namespace_lock_subprocess_helper() {
    let Some(mode) = std::env::var_os(NAMESPACE_LOCK_CHILD_MODE_ENV) else {
        return;
    };
    let path = PathBuf::from(
        std::env::var_os(NAMESPACE_LOCK_CHILD_PATH_ENV)
            .expect("namespace-lock child path is configured"),
    );
    match mode.to_str().expect("namespace-lock child mode is UTF-8") {
        "expect_contended" => {
            let error = CheckpointStore::open(StoreOptions {
                ownership_timeout: Duration::from_millis(100),
                ownership_retry_interval: Duration::from_millis(10),
                ..options(&path)
            })
            .expect_err("the parent process owns the namespace");
            assert!(matches!(error, StoreError::NamespaceLocked { .. }));
        }
        "acquire_release" => {
            drop(open(&path));
        }
        "exit_while_held" => {
            let _owner = open(&path);
            fs::write(path.join("child-held-lock"), b"held")
                .expect("child records successful lock acquisition");
            std::process::exit(17);
        }
        other => panic!("unknown namespace-lock child mode `{other}`"),
    }
}

/// Scenario: one process owns a checkpoint namespace while a second local
/// process attempts to open it, then retries after the owner releases it.
/// Guarantees: ownership conflicts across processes and a later process can
/// acquire the same namespace after release.
#[test]
fn namespace_ownership_conflicts_across_processes_and_releases() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("namespace");
    let owner = open(&path);

    let contended = run_namespace_lock_child(&path, "expect_contended");
    assert!(
        contended.success(),
        "child process did not observe namespace contention"
    );

    drop(owner);
    let acquired = run_namespace_lock_child(&path, "acquire_release");
    assert!(
        acquired.success(),
        "child process could not acquire the released namespace"
    );
}

/// Scenario: a child process terminates without running destructors while it
/// owns the checkpoint namespace lock.
/// Guarantees: process teardown releases the operating-system lock so a
/// successor can immediately recover the namespace.
#[test]
fn abnormal_process_exit_releases_namespace_lock() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("namespace");

    let status = run_namespace_lock_child(&path, "exit_while_held");
    assert_eq!(status.code(), Some(17));
    assert!(
        path.join("child-held-lock").is_file(),
        "child did not prove that it acquired the namespace"
    );

    let successor = open(&path);
    assert_eq!(successor.generation(), 0);
}

/// Scenario: a cancellable store open is waiting behind a live namespace
/// owner when shutdown is asserted and the owner then releases the lock.
/// Guarantees: the cancelled waiter returns without acquiring or cleaning
/// the namespace, and an uncancelled successor can recover it immediately.
#[test]
fn cancelled_namespace_waiter_never_acquires_or_mutates_after_release() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let owner = open(&path);
    let stale_temp = path.join(CURRENT_COMPACT_TEMP_FILE_NAME);
    write_bytes(&stale_temp, b"stale");
    let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let waiter_cancelled = std::sync::Arc::clone(&cancelled);
    let (check_tx, check_rx) = std::sync::mpsc::channel();
    let contended = StoreOptions {
        ownership_timeout: Duration::from_secs(5),
        ownership_retry_interval: Duration::from_millis(10),
        ..options(&path)
    };
    let waiter = std::thread::spawn(move || {
        CheckpointStore::open_cancellable(contended, || {
            let _ = check_tx.send(());
            waiter_cancelled.load(std::sync::atomic::Ordering::Acquire)
        })
    });

    for _ in 0..7 {
        check_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("waiter did not reach its first contended lock attempt");
    }
    cancelled.store(true, std::sync::atomic::Ordering::Release);
    drop(owner);

    let opened = waiter
        .join()
        .expect("waiter thread joins")
        .expect("open succeeds");
    assert!(opened.is_none());
    assert!(stale_temp.exists());

    let successor = open(&path);
    assert_eq!(successor.generation(), 0);
    assert!(!stale_temp.exists());
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
            register.file_id = FileId::from_bytes(bytes);
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

/// Scenario: a namespace reaches exactly two transactions, then receives a
/// third append.
/// Guarantees: equality stays in the current WAL; the third append compacts
/// first and becomes sequence one in the next generation.
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
    assert_eq!(store.generation(), 0);
    let progressed = store
        .commit_progress(vec![progress(1, 256, 512)])
        .expect("third transaction succeeds after pre-append compaction");
    assert_eq!(store.generation(), 1);
    assert_eq!(progressed[0].sequence, 1);
    assert_eq!(store.stats().wal_transactions, 1);
    assert_eq!(store.stats().preappend_compactions, 1);
    assert!(!store.compaction_due());
    assert!(!store.compact_if_due().expect("no compaction is due"));
    assert_eq!(committed_offset(&store, 1), 512);
    drop(store);

    let reopened = open(&path);
    assert_eq!(reopened.generation(), 1);
    assert_eq!(reopened.recovery().snapshot_records, 1);
    assert_eq!(reopened.recovery().transactions_replayed, 1);
    assert_eq!(committed_offset(&reopened, 1), 512);
}

/// Scenario: a transaction-count-triggered pre-append compaction fails at
/// every publication boundary.
/// Guarantees: the triggering transaction is never appended; restart selects
/// the complete old or compacted generation containing only preexisting state.
#[test]
fn preappend_compaction_fault_never_appends_triggering_transaction() {
    for point in FaultPoint::PUBLICATION {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("namespace");
        let configured = || StoreOptions {
            compact_after_transactions: 1,
            ..options(&path)
        };
        let mut seeded = CheckpointStore::open(configured()).unwrap();
        let _registered = seeded.register_files(vec![registration(1)]).unwrap();
        drop(seeded);

        let mut store = CheckpointStore::open_with_fault(configured(), point).unwrap();
        let error = store.register_files(vec![registration(2)]).unwrap_err();
        assert!(matches!(
            error,
            StoreError::InjectedFault { point: fired } if fired == point
        ));
        assert!(store.table().get(&file_id(1)).is_some());
        assert!(store.table().get(&file_id(2)).is_none());
        drop(store);

        let marker_replaced = matches!(
            point,
            FaultPoint::AfterMarkerPublish
                | FaultPoint::BeforeMarkerDirSync
                | FaultPoint::AfterMarkerDirSync
        );
        let reopened = CheckpointStore::open(configured()).unwrap();
        assert_eq!(reopened.generation(), u64::from(marker_replaced));
        assert!(reopened.table().get(&file_id(1)).is_some());
        assert!(reopened.table().get(&file_id(2)).is_none());
    }
}

/// Scenario: the complete-WAL byte threshold is tested immediately below and
/// exactly at one prospective transaction boundary.
/// Guarantees: the 56-byte header participates in arithmetic, equality is
/// accepted, and one additional byte requires pre-append compaction.
#[test]
fn byte_compaction_threshold_ignores_an_empty_wal_header() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let compact_after_bytes = minimum_compact_after_bytes().unwrap();
    let mut store = CheckpointStore::open(StoreOptions {
        compact_after_bytes,
        compact_after_transactions: u32::MAX,
        ..options(&path)
    })
    .expect("namespace opens");

    assert_eq!(store.stats().wal_bytes, WAL_HEADER_LEN as u64);
    assert!(!store.compaction_due());
    assert!(!store.compact_if_due().expect("empty WAL is not due"));

    let transaction_bytes = encode_transaction(&Transaction {
        sequence: 1,
        operations: vec![Operation::RegisterFile(registration(1))],
    })
    .unwrap()
    .len() as u64;
    store.wal_bytes = compact_after_bytes - transaction_bytes;
    assert!(!store.append_requires_compaction(transaction_bytes).unwrap());
    store.wal_bytes += 1;
    assert!(store.append_requires_compaction(transaction_bytes).unwrap());
    store.wal_bytes = compact_after_bytes;
    assert!(store.compaction_due());
}

/// Scenario: cancellation becomes visible after a due compaction stages its
/// complete snapshot/WAL pair but before it publishes `CURRENT`.
/// Guarantees: compaction does not start marker publication, keeps the live
/// handle on the old generation, and restart still selects the old state.
#[test]
fn compaction_cancellation_stops_before_marker_publication() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let next_snapshot = path.join(snapshot_file_name(1));
    let next_wal = path.join(wal_file_name(1));
    let mut store = CheckpointStore::open(StoreOptions {
        compact_after_transactions: 1,
        ..options(&path)
    })
    .expect("namespace opens");
    let _registered = store
        .register_files(vec![registration(1)])
        .expect("registers");

    let outcome = store
        .compact_if_due_cancellable(|| next_snapshot.is_file() && next_wal.is_file())
        .expect("cancellation is not a compaction error");

    assert!(outcome.is_none());
    assert_eq!(store.generation(), 0);
    assert_eq!(committed_offset(&store, 1), 0);
    drop(store);

    let reopened = open(&path);
    assert_eq!(reopened.generation(), 0);
    assert_eq!(committed_offset(&reopened, 1), 0);
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
        defaults.compact_after_transactions,
        defaults.max_tracked_files,
        defaults.fingerprint_bytes,
    )
    .expect("the default configuration is representable");
    let store = open(&path);
    assert_eq!(store.limits(), expected);
    assert!(expected.max_snapshot_bytes <= ARTIFACT_BYTES_CEILING);
    assert!(expected.max_wal_bytes <= ARTIFACT_BYTES_CEILING);
    assert_eq!(expected.max_wal_bytes, defaults.compact_after_bytes);
    assert_eq!(
        expected.max_transaction_bytes + WAL_HEADER_LEN as u64,
        minimum_compact_after_bytes().unwrap()
    );
    assert_eq!(
        expected.max_wal_transactions,
        u64::from(defaults.compact_after_transactions)
    );
    drop(store);

    let too_small = dir.path().join("too-small-wal");
    let refused = CheckpointStore::open(StoreOptions {
        compact_after_bytes: minimum_compact_after_bytes().unwrap() - 1,
        ..options(&too_small)
    })
    .expect_err("a byte threshold smaller than one fresh transaction is refused");
    assert!(matches!(
        refused,
        StoreError::ResourceBounds {
            source: super::limits::LimitsError::CompactAfterBytesTooSmall { .. },
            ..
        }
    ));
    assert!(!too_small.exists(), "the namespace must not be created");

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

/// Scenario: test-only accounting places a valid WAL one byte past the point
/// where the next progress transaction would fit its complete-WAL threshold.
/// Guarantees: append compacts first, writes sequence one to a fresh bounded
/// WAL, and recovery observes the complete updated state.
#[test]
fn crossing_the_compaction_threshold_keeps_the_wal_within_its_recovery_cap() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let compact_after_bytes = minimum_compact_after_bytes().unwrap();
    let mut store = CheckpointStore::open(StoreOptions {
        compact_after_bytes,
        compact_after_transactions: u32::MAX,
        sync_interval: NEVER_ELAPSES,
        ..options(&path)
    })
    .expect("namespace opens");
    let _registered = store
        .register_files(vec![registration(1)])
        .expect("registers");
    let update = progress(1, 0, 64);
    let transaction_bytes = encode_transaction(&Transaction {
        sequence: store.stats().next_sequence,
        operations: vec![Operation::UpdateProgress(update.clone())],
    })
    .unwrap()
    .len() as u64;
    store.wal_bytes = compact_after_bytes - transaction_bytes + 1;
    let outcomes = store.commit_progress(vec![update]).unwrap();
    assert_eq!(store.generation(), 1);
    assert_eq!(outcomes[0].sequence, 1);
    assert_eq!(store.stats().wal_transactions, 1);
    assert_eq!(store.stats().preappend_compactions, 1);
    assert!(store.stats().wal_bytes <= compact_after_bytes);
    assert_eq!(committed_offset(&store, 1), 64);
    drop(store);

    let reopened = CheckpointStore::open(StoreOptions {
        compact_after_bytes,
        compact_after_transactions: u32::MAX,
        sync_interval: NEVER_ELAPSES,
        ..options(&path)
    })
    .unwrap();
    assert_eq!(reopened.generation(), 1);
    assert_eq!(committed_offset(&reopened, 1), 64);
}

/// Scenario: an invalid progress operation is submitted when its encoded
/// transaction would require pre-append compaction.
/// Guarantees: deterministic replay validation fails before compaction, so
/// invalid input cannot publish a new generation as a side effect.
#[test]
fn invalid_append_is_rejected_before_required_compaction() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let compact_after_bytes = minimum_compact_after_bytes().unwrap();
    let mut store = CheckpointStore::open(StoreOptions {
        compact_after_bytes,
        ..options(&path)
    })
    .unwrap();
    let _registered = store.register_files(vec![registration(1)]).unwrap();
    let stale = progress(1, 99, 100);
    let transaction_bytes = encode_transaction(&Transaction {
        sequence: store.stats().next_sequence,
        operations: vec![Operation::UpdateProgress(stale.clone())],
    })
    .unwrap()
    .len() as u64;
    store.wal_bytes = compact_after_bytes - transaction_bytes + 1;
    let generation_before = store.generation();
    let table_before = store.table().clone();

    let error = store.commit_progress(vec![stale]).unwrap_err();
    assert!(matches!(error, StoreError::Apply { .. }));
    assert_eq!(store.generation(), generation_before);
    assert_eq!(store.stats().preappend_compactions, 0);
    assert_eq!(store.table(), &table_before);
}

/// Scenario: a valid two-transaction WAL is reopened with a transaction
/// threshold of one while its byte length remains below the derived byte cap.
/// Guarantees: recovery enforces the interacting transaction-count maximum
/// independently of artifact length and never replays the extra transaction.
#[test]
fn recovery_rejects_wal_past_transaction_threshold() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = CheckpointStore::open(StoreOptions {
        compact_after_transactions: 2,
        ..options(&path)
    })
    .unwrap();
    let _registered = store.register_files(vec![registration(1)]).unwrap();
    let _progressed = store.commit_progress(vec![progress(1, 0, 64)]).unwrap();
    assert_eq!(store.stats().wal_transactions, 2);
    drop(store);

    let error = CheckpointStore::open(StoreOptions {
        compact_after_transactions: 1,
        ..options(&path)
    })
    .unwrap_err();
    assert!(matches!(
        error,
        StoreError::RecoveredWalTransactionsExceedMaximum {
            transactions: 2,
            max: 1,
            ..
        }
    ));
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

/// Scenario: an authenticated WAL transaction header declares a body larger
/// than the codec and store per-transaction bound.
/// Guarantees: recovery rejects the declared body before slicing or decoding
/// operations, preserving the one-bounded-transaction memory assumption.
#[test]
fn oversized_wal_transaction_is_rejected_after_header_validation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    drop(open(&path));
    let limits = options(&path)
        .limits()
        .expect("the default bounds are representable");
    // `transaction_bytes = body_len + 36-byte header + 4-byte frame CRC`.
    let declared_body = u32::try_from(limits.max_transaction_bytes - 39)
        .expect("the default transaction bound fits u32");
    let mut bytes = encode_wal_header(0, NAMESPACE_ID).expect("the WAL header encodes");
    let valid = encode_transaction(&Transaction {
        sequence: 1,
        operations: vec![Operation::RegisterFile(registration(1))],
    })
    .expect("the seed transaction encodes");
    let mut header = valid[..36].to_vec();
    header[20..24].copy_from_slice(&declared_body.to_be_bytes());
    header[24..28].copy_from_slice(&(!declared_body).to_be_bytes());
    let header_crc = crc32c(&header[..32]);
    header[32..36].copy_from_slice(&header_crc.to_be_bytes());
    bytes.extend_from_slice(&header);
    write_bytes(&path.join(wal_file_name(0)), &bytes);

    let error =
        CheckpointStore::open(options(&path)).expect_err("the oversized transaction is refused");
    assert!(matches!(
        error,
        StoreError::Decode {
            artifact: "WAL",
            source: DecodeError::TransactionBodyOutOfBounds {
                sequence: 1,
                len,
                max,
                ..
            },
            ..
        } if len == u64::from(declared_body) && max + 40 == limits.max_transaction_bytes
    ));
}

/// Scenario: an underlying reader returns a valid transaction in repeated
/// short reads, and its first chunk alone is structurally `Incomplete`.
/// Guarantees: the bounded store reader continues until physical EOF and
/// exposes only the complete bytes, so a short read cannot authorize WAL
/// truncation or discard.
#[test]
fn short_reads_are_drained_to_eof_before_wal_classification() {
    struct ShortReader<'a> {
        remaining: &'a [u8],
        max_read: usize,
        reads: usize,
    }

    impl std::io::Read for ShortReader<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.remaining.is_empty() {
                return Ok(0);
            }
            let len = self.remaining.len().min(buffer.len()).min(self.max_read);
            buffer[..len].copy_from_slice(&self.remaining[..len]);
            self.remaining = &self.remaining[len..];
            self.reads += 1;
            Ok(len)
        }
    }

    let transaction = Transaction {
        sequence: 1,
        operations: vec![Operation::RegisterFile(registration(1))],
    };
    let encoded = encode_transaction(&transaction).expect("the transaction encodes");
    assert!(matches!(
        scan_next_transaction(&encoded[..10], 1).expect("the short prefix scans"),
        Some(TransactionScan::Incomplete { bytes: 10 })
    ));

    let mut reader = ShortReader {
        remaining: &encoded,
        max_read: 7,
        reads: 0,
    };
    let path = Path::new("short-read-wal");
    let complete = super::fsio::read_bounded_reader(
        &mut reader,
        encoded.len(),
        path,
        "WAL transaction",
        u64::try_from(encoded.len()).expect("transaction length fits u64"),
    )
    .expect("short reads are accumulated through EOF");
    assert!(reader.reads > 1);
    assert_eq!(complete, encoded);
    assert!(matches!(
        scan_next_transaction(&complete, 1).expect("the complete transaction scans"),
        Some(TransactionScan::Complete {
            transaction: decoded,
            consumed,
        }) if decoded == transaction && consumed == complete.len()
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

/// Scenario: checkpoint recovery cancellation becomes visible immediately
/// after the first chunk of a multi-chunk artifact read.
/// Guarantees: the bounded reader returns its cancellation outcome before
/// issuing a second read or returning a partial artifact as complete.
#[test]
fn artifact_read_cancellation_stops_between_chunks() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("artifact");
    let bytes = vec![0x5a; 16 * 1024];
    write_bytes(&path, &bytes);
    let file = fs::File::open(&path).expect("artifact opens");
    let mut cancellation_checks = 0usize;

    let result = super::fsio::read_bounded_contents_cancellable(
        file,
        bytes.len(),
        &path,
        "test artifact",
        bytes.len() as u64,
        &mut || {
            cancellation_checks += 1;
            cancellation_checks >= 3
        },
    )
    .expect("cancellation is not a read error");

    assert!(result.is_none());
    assert_eq!(cancellation_checks, 3);
}

/// Scenario: cancellation becomes visible after the first of two missing
/// checkpoint namespace directories is created.
/// Guarantees: namespace creation returns its cancellation outcome without
/// issuing the next directory-creation operation.
#[test]
fn namespace_creation_cancellation_stops_between_directories() {
    let dir = tempfile::tempdir().expect("temp dir");
    let first = dir.path().join("first");
    let namespace = first.join("namespace");
    let mut cancellation_checks = 0usize;
    let mut faults = FaultPlan::disabled();

    let result =
        super::fsio::create_namespace_dir_cancellable(&namespace, &mut faults, &mut || {
            cancellation_checks += 1;
            first.is_dir()
        })
        .expect("cancellation is not a namespace creation error");

    assert!(result.is_none());
    assert!(cancellation_checks > 0);
    assert!(first.is_dir());
    assert!(!namespace.exists());
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
    let encoded = encode_snapshot(1, NAMESPACE_ID, &store.table().snapshot_records())
        .expect("the current table encodes");
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

/// Scenario: an authenticated empty snapshot header declares one record even
/// though its bytes contain no record frame, while the configured population
/// limit permits that count.
/// Guarantees: store recovery propagates the codec's physical-count guard
/// before allocating or decoding any snapshot record.
#[test]
fn physically_impossible_snapshot_count_fails_before_state_allocation() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    drop(open(&path));

    let mut snapshot = encode_snapshot(0, NAMESPACE_ID, &[]).expect("empty snapshot encodes");
    snapshot[52..56].copy_from_slice(&1u32.to_be_bytes());
    let header_crc = crc32c(&snapshot[..56]);
    snapshot[56..60].copy_from_slice(&header_crc.to_be_bytes());
    write_bytes(&path.join(snapshot_file_name(0)), &snapshot);

    assert!(matches!(
        CheckpointStore::open(StoreOptions {
            max_tracked_files: 8,
            ..options(&path)
        })
        .expect_err("the impossible authenticated count fails closed"),
        StoreError::Decode {
            artifact: "snapshot",
            source: DecodeError::SnapshotRecordCountExceedsPhysicalMaximum {
                declared: 1,
                max: 0,
                ..
            },
            ..
        }
    ));
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

/// Scenario: truncate and quarantine resets carry replacement fingerprints
/// wider than the configured identity evidence window.
/// Guarantees: both reset paths fail before WAL append or table mutation, so
/// epoch-changing operations cannot bypass configured recovery admission.
#[test]
fn configured_fingerprint_limit_applies_to_every_reset() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("namespace");
    let mut store = CheckpointStore::open(StoreOptions {
        fingerprint_bytes: 8,
        ..options(&path)
    })
    .unwrap();
    let _registered = store
        .register_files(vec![registration(1), registration(2)])
        .unwrap();
    let _quarantined = store.quarantine_files(vec![quarantine(2)]).unwrap();
    let before = store.table().clone();
    let sequence_before = store.stats().next_sequence;

    let error = store
        .reset_after_truncate(ResetAfterTruncate {
            file_id: file_id(1),
            expected_active_epoch: 1,
            observed_truncated_size: 0,
            resulting_epoch: 2,
            new_committed_offset: 0,
            new_framing_resume: FramingResume::Clean,
            new_fingerprint: vec![1; 9],
            reset_time_unix_nano: 3,
            reason_code: TRUNCATE_RESET_REASON_READ_NEW,
        })
        .unwrap_err();
    assert!(matches!(
        error,
        StoreError::FingerprintExceedsConfiguredMaximum {
            context: "truncate reset fingerprint",
            len: 9,
            max: 8,
            ..
        }
    ));

    let error = store
        .reset_quarantined_file(ResetQuarantinedFile {
            file_id: file_id(2),
            expected_quarantine_epoch: 1,
            action: ResetQuarantineAction::ResetToBeginning,
            resulting_epoch: 2,
            resulting_offset: 0,
            new_committed_frontier_guard: CommittedFrontierGuard::empty(),
            new_framing_resume: FramingResume::Clean,
            new_fingerprint: vec![2; 9],
            action_time_unix_nano: 3,
            namespace_id: NAMESPACE_ID.to_owned(),
            audit_reason: "release".to_owned(),
        })
        .unwrap_err();
    assert!(matches!(
        error,
        StoreError::FingerprintExceedsConfiguredMaximum {
            context: "quarantine reset fingerprint",
            len: 9,
            max: 8,
            ..
        }
    ));
    assert_eq!(store.table(), &before);
    assert_eq!(store.stats().next_sequence, sequence_before);
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
    assert!(!path.join(CURRENT_COMPACT_TEMP_FILE_NAME).exists());
    drop(store);

    let reopened = open(&path);
    assert_eq!(reopened.generation(), 1);
    assert_eq!(reopened.table().len(), 1);
}

/// Scenario: Windows reports each documented `ReplaceFileW` partial-progress
/// failure class while no non-normative backup artifact is used.
/// Guarantees: every class that may have removed or renamed `CURRENT` is
/// treated as authority-uncertain; an ordinary access denial is not.
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
    assert!(super::fsio::windows_replace_failure_may_have_changed(
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
            FaultPoint::AfterRetiredWalRemoval => {
                assert!(path.join(snapshot_file_name(0)).is_file());
                assert!(!path.join(wal_file_name(0)).exists());
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

/// Scenario: each reason value reserved from version-1 encoder output is
/// supplied through every public quarantine or administrative removal path.
/// Guarantees: quarantine rejects `0x0000` and `0x0004`, removal rejects
/// `0x0000`, and no refusal advances durable or in-memory state.
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

    let expect_quarantine_field = |error: StoreError, expected| match error {
        StoreError::ReservedReasonCode { field, reason_code } => {
            assert_eq!(field, "quarantine_file.reason_code");
            assert_eq!(reason_code, expected);
        }
        other => panic!("expected a reserved reason code refusal, got {other:?}"),
    };
    for reason_code in [REASON_CODE_RESERVED, 0x0004] {
        let mut reserved_quarantine = quarantine(1);
        reserved_quarantine.reason_code = reason_code;
        expect_quarantine_field(
            store
                .quarantine_files(vec![reserved_quarantine.clone()])
                .expect_err("quarantine_files refuses the reserved code"),
            reason_code,
        );
        expect_quarantine_field(
            store
                .append(vec![Operation::QuarantineFile(reserved_quarantine)])
                .expect_err("append refuses the reserved code"),
            reason_code,
        );
    }

    let expect_removal_field = |error: StoreError| match error {
        StoreError::ReservedReasonCode { field, reason_code } => {
            assert_eq!(field, "remove_file.removal_reason");
            assert_eq!(reason_code, REASON_CODE_RESERVED);
        }
        other => panic!("expected a reserved reason code refusal, got {other:?}"),
    };
    expect_removal_field(
        store
            .append(vec![Operation::RemoveFile(removal(
                1,
                REASON_CODE_RESERVED,
            ))])
            .expect_err("append refuses the reserved code"),
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

    // A non-reserved administrative code still works.
    let outcome = store
        .remove_quarantined_file(file_id(2), 0x0008, 10_000_000_000, "purge".to_owned())
        .expect("a valid administrative removal is accepted")
        .expect("the record was present");
    assert!(outcome.synced);
    assert_eq!(store.table().len(), 1);
}

/// Scenario: a checksum-valid WAL contains a quarantine using `0x0000` or
/// `0x0004`, or a removal using reserved `0x0000`.
/// Guarantees: structural decoding accepts the opaque `u16`, then store
/// recovery rejects it before applying the operation.
#[test]
fn recovered_wal_rejects_every_reserved_reason_code() {
    for (name, operation, reason_offset, reserved_reason, expected_field) in [
        (
            "quarantine-zero",
            Operation::QuarantineFile(quarantine(1)),
            1 + 16 + 4,
            REASON_CODE_RESERVED,
            "quarantine_file.reason_code",
        ),
        (
            "quarantine-four",
            Operation::QuarantineFile(quarantine(1)),
            1 + 16 + 4,
            0x0004,
            "quarantine_file.reason_code",
        ),
        (
            "removal",
            Operation::RemoveFile(removal(1, 0x0001)),
            1 + 16 + 4 + 1,
            REASON_CODE_RESERVED,
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

        let mut transaction = encode_transaction(&Transaction {
            sequence: 2,
            operations: vec![operation],
        })
        .expect("the structurally valid transaction encodes");
        patch_first_transaction_operation_u16(&mut transaction, reason_offset, reserved_reason);
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
                reason_code,
                ..
            } if found == file_id(1)
                && field == expected_field
                && reason_code == reserved_reason
        ));
    }
}

/// Scenario: a checksum-valid snapshot contains zero or encoder-reserved
/// quarantine evidence and its WAL immediately resets or removes that record.
/// Guarantees: zero violates the snapshot reachable-state invariant, while
/// `0x0004` remains structurally opaque but is rejected by store policy before
/// replay can erase the evidence.
#[test]
fn recovered_snapshot_rejects_reserved_reason_before_wal_can_erase_it() {
    let reset = Operation::ResetQuarantinedFile(ResetQuarantinedFile {
        file_id: file_id(1),
        expected_quarantine_epoch: 1,
        action: ResetQuarantineAction::ResetToBeginning,
        resulting_epoch: 2,
        resulting_offset: 0,
        new_committed_frontier_guard: zero_guard(0),
        new_framing_resume: FramingResume::Clean,
        new_fingerprint: vec![1; 8],
        action_time_unix_nano: 4_000,
        namespace_id: NAMESPACE_ID.to_owned(),
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

    for (name, operation, reserved_reason) in [
        ("reset-zero", reset.clone(), REASON_CODE_RESERVED),
        ("reset-four", reset, 0x0004),
        ("removal-zero", removal.clone(), REASON_CODE_RESERVED),
        ("removal-four", removal, 0x0004),
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
        store.compact().expect("quarantine snapshot is published");
        let snapshot_records = records(&store);
        drop(store);

        let mut snapshot = encode_snapshot(1, NAMESPACE_ID, &snapshot_records)
            .expect("the valid snapshot encodes");
        patch_first_snapshot_quarantine_reason(&mut snapshot, reserved_reason);
        write_bytes(&path.join(snapshot_file_name(1)), &snapshot);
        let transaction = encode_transaction(&Transaction {
            sequence: 1,
            operations: vec![operation],
        })
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
        if reserved_reason == REASON_CODE_RESERVED {
            assert!(matches!(
                error,
                StoreError::Decode {
                    source: DecodeError::InvalidSnapshotState {
                        file_id: found,
                        ..
                    },
                    ..
                } if found == file_id(1)
            ));
        } else {
            assert!(matches!(
                error,
                StoreError::ReservedReasonCodeRecovered {
                    file_id: found,
                    field: "quarantine_file.reason_code",
                    reason_code,
                    ..
                } if found == file_id(1) && reason_code == reserved_reason
            ));
        }
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
/// largest recoverable fingerprint window and tracked-file population, the
/// largest recoverable byte threshold, and the largest transaction threshold.
/// Guarantees: options carry the resolved namespace identity and all four
/// durable-size knobs from one validated configuration, and every configuration
/// that validates can create a namespace, write to it, and recover it, so
/// no legal configuration -- including each size boundary -- produces a
/// namespace its own store cannot reopen.
#[test]
fn every_legal_configuration_creates_a_namespace_that_reopens() {
    let default_tracked_files = LimitsConfig::default().max_tracked_files;
    let default_fingerprint_bytes = IdentityConfig::default().fingerprint_bytes;
    let default_compact_after_bytes = CheckpointConfig::default().compact_after_bytes;
    let default_compact_after_transactions = CheckpointConfig::default().compact_after_transactions;
    let boundary_files = u32::try_from(largest_accepted(
        u64::from(default_tracked_files),
        u64::from(u32::MAX),
        |candidate| {
            StoreLimits::derive(
                default_compact_after_bytes,
                default_compact_after_transactions,
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
            StoreLimits::derive(
                candidate,
                default_compact_after_transactions,
                default_tracked_files,
                default_fingerprint_bytes,
            )
            .is_ok()
        },
    );
    let boundary_fingerprint = largest_accepted(
        default_fingerprint_bytes,
        FINGERPRINT_MAX_BYTES as u64,
        |candidate| {
            StoreLimits::derive(
                default_compact_after_bytes,
                default_compact_after_transactions,
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
        (
            "largest-transaction-threshold",
            Box::new(|config: &mut Config| {
                config.checkpoint.compact_after_transactions = u32::MAX;
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
        assert_eq!(
            options.compact_after_transactions,
            runtime.checkpoint.compact_after_transactions
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
