// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;

use tempfile::{TempDir, tempdir};

use super::matcher::{
    CandidateInventory, FileIdSource, IdentityMatch, IdentityResolution, IdentitySettings,
    ResolvedIdentity, resolve_and_persist as resolve_with_inventory,
    resolve_and_persist_with_admission, resolve_and_persist_with_admission_cancellable,
    resolve_and_persist_with_source as resolve_with_inventory_and_source,
};
use super::*;
use crate::receivers::filelog_receiver::checkpoint::primitives::{
    AdvisoryPath, CommittedFrontierGuard, CommittedFrontierWindow, FRAMING_PROFILE_VERSION,
    FramingResume, LifecycleState, QUARANTINE_REASON_RECOVERY_MISMATCH,
    WAL_MAX_NON_PROGRESS_OPS_PER_TX,
};
use crate::receivers::filelog_receiver::checkpoint::store::fault::FaultPoint;
use crate::receivers::filelog_receiver::checkpoint::store::{CheckpointStore, StoreOptions};
use crate::receivers::filelog_receiver::checkpoint::wal::{
    Operation, QuarantineFile, RegisterFile,
};
use crate::receivers::filelog_receiver::config::{OnRecoveryMismatch, StartAt};

const DIGEST: [u8; 32] = [0x44; 32];

/// Test-only zero-filled window guard: a deterministic, obviously-fake
/// `CommittedFrontierGuard` for tests that only need a structurally valid
/// guard and do not exercise real continuity evidence. Production code
/// must never do this; see
/// `crate::receivers::filelog_receiver::checkpoint::primitives::CommittedFrontierWindow`
/// for the real, non-fabricated runtime window.
fn zero_guard(committed_offset: u64) -> CommittedFrontierGuard {
    let window_len = committed_offset.min(64) as usize;
    CommittedFrontierGuard::compute(committed_offset, &vec![0u8; window_len]).unwrap()
}

/// Test-only zero-filled committed-frontier window: a deterministic,
/// obviously-fake window for tests that only need a structurally valid
/// value and do not exercise real continuity evidence.
fn zero_window(end_offset: u64) -> CommittedFrontierWindow {
    let window_len = end_offset.min(64) as usize;
    CommittedFrontierWindow::new(end_offset, vec![0u8; window_len]).unwrap()
}

fn locator(number: u64) -> Locator {
    Locator::PosixDevIno {
        dev: 7,
        ino: number,
    }
}

fn evidence(locator: Locator, size: u64, fingerprint: &[u8], path: &[u8]) -> CandidateEvidence {
    CandidateEvidence {
        locator,
        size,
        fingerprint: fingerprint.to_vec(),
        advisory_path: AdvisoryPath::from_unix_bytes(path).unwrap(),
        committed_frontier_window: zero_window(size),
    }
}

/// Builds evidence carrying a real, distinguishable (non-zero-filled)
/// committed-frontier window ending at `size`, so a test can tell real
/// evidence apart from a fabricated placeholder.
fn evidence_with_window(
    locator: Locator,
    size: u64,
    fingerprint: &[u8],
    path: &[u8],
) -> CandidateEvidence {
    let window_len = size.min(64) as usize;
    let bytes: Vec<u8> = (0..window_len).map(|index| (index + 1) as u8).collect();
    CandidateEvidence {
        locator,
        size,
        fingerprint: fingerprint.to_vec(),
        advisory_path: AdvisoryPath::from_unix_bytes(path).unwrap(),
        committed_frontier_window: CommittedFrontierWindow::new(size, bytes).unwrap(),
    }
}

fn settings() -> IdentitySettings {
    IdentitySettings {
        fingerprint_bytes: 4,
        ignored_header_bytes: 0,
        start_at: StartAt::Beginning,
        on_recovery_mismatch: OnRecoveryMismatch::Beginning,
        framing_profile_version: FRAMING_PROFILE_VERSION,
        framing_profile_digest: DIGEST,
        max_candidates: 32,
        max_inventory_candidates: 64,
        max_tracked_files: 32,
    }
}

fn test_store(max_tracked_files: u32) -> (TempDir, CheckpointStore, StoreOptions) {
    let directory = tempdir().unwrap();
    let mut options = StoreOptions::new(
        directory.path().join("checkpoint"),
        "identity-test".to_owned(),
    );
    options.max_tracked_files = max_tracked_files;
    options.fingerprint_bytes = 16;
    let store = CheckpointStore::open(options.clone()).unwrap();
    (directory, store, options)
}

/// Scenario: one reconciliation contains two genuinely new candidates while
/// durable tracked-file capacity has one remaining slot.
/// Guarantees: candidate order selects exactly one durable registration, the
/// other outcome is Deferred, and no over-capacity operation reaches the WAL.
#[test]
fn admission_resolution_defers_new_identity_at_durable_capacity() {
    let (_directory, mut store, _options) = test_store(1);
    let candidates = vec![
        evidence(locator(1), 4, b"aaaa", b"/a.log"),
        evidence(locator(2), 4, b"bbbb", b"/b.log"),
    ];
    let inventory =
        CandidateInventory::from_complete_reconciliation(&candidates, &HashSet::new(), 4);
    let mut settings = settings();
    settings.max_tracked_files = 1;

    let resolutions =
        resolve_and_persist_with_admission(&mut store, &candidates, &inventory, &settings, 10)
            .unwrap();
    assert!(matches!(resolutions[0], IdentityResolution::Resolved(_)));
    assert_eq!(resolutions[1], IdentityResolution::Deferred);
    assert_eq!(store.table().len(), 1);
    assert_eq!(store.stats().wal_transactions, 1);
}

/// Scenario: cancellation becomes visible after identity planning but before
/// a reconciliation can append its atomic registration group.
/// Guarantees: the cancellable resolver returns no runtime resolutions and
/// leaves both the checkpoint table and WAL transaction count unchanged.
#[test]
fn admission_resolution_cancellation_preempts_persistence() {
    let (_directory, mut store, _options) = test_store(1);
    let candidates = vec![evidence(locator(1), 4, b"aaaa", b"/a.log")];
    let inventory =
        CandidateInventory::from_complete_reconciliation(&candidates, &HashSet::new(), 4);
    let mut settings = settings();
    settings.max_tracked_files = 1;

    let resolutions = resolve_and_persist_with_admission_cancellable(
        &mut store,
        &candidates,
        &inventory,
        &settings,
        10,
        || true,
    )
    .unwrap();

    assert!(resolutions.is_none());
    assert!(store.table().is_empty());
    assert_eq!(store.stats().wal_transactions, 0);
}

/// Scenario: one identity reconciliation spans two WAL transactions and
/// cancellation becomes visible after the first transaction is durable.
/// Guarantees: the resolver exposes no partial runtime resolutions while
/// restart recovers exactly the completed registration prefix.
#[test]
fn admission_resolution_cancellation_hides_durable_transaction_prefix() {
    let candidate_count = usize::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX) + 1;
    let (_directory, mut store, options) = test_store(candidate_count as u32);
    let candidates: Vec<CandidateEvidence> = (0..candidate_count)
        .map(|index| {
            evidence(
                locator(index as u64 + 1),
                4,
                b"aaaa",
                format!("/candidate-{index}.log").as_bytes(),
            )
        })
        .collect();
    let inventory =
        CandidateInventory::from_complete_reconciliation(&candidates, &HashSet::new(), 4);
    let mut settings = settings();
    settings.max_candidates = candidate_count;
    settings.max_inventory_candidates = candidate_count;
    settings.max_tracked_files = candidate_count;
    let mut cancellation_checks = 0usize;

    let resolutions = resolve_and_persist_with_admission_cancellable(
        &mut store,
        &candidates,
        &inventory,
        &settings,
        10,
        || {
            let cancelled = cancellation_checks >= 2;
            cancellation_checks += 1;
            cancelled
        },
    )
    .unwrap();

    assert!(resolutions.is_none());
    assert_eq!(
        store.table().len(),
        usize::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX)
    );
    assert_eq!(store.stats().wal_transactions, 1);
    assert_eq!(cancellation_checks, 3);
    drop(store);

    let recovered = CheckpointStore::open(options).unwrap();
    assert_eq!(
        recovered.table().len(),
        usize::from(WAL_MAX_NON_PROGRESS_OPS_PER_TX)
    );
}

fn registration(
    file_id: FileId,
    locator: Locator,
    offset: u64,
    fingerprint: &[u8],
    path: &[u8],
) -> Operation {
    Operation::RegisterFile(RegisterFile {
        file_id,
        file_epoch: 1,
        committed_offset: offset,
        committed_frontier_guard: zero_guard(offset),
        fingerprint: fingerprint.to_vec(),
        ignored_header_bytes: 0,
        locator,
        framing_profile_version: FRAMING_PROFILE_VERSION,
        framing_profile_digest: DIGEST,
        framing_resume: FramingResume::Clean,
        last_seen_time_unix_nano: 1,
        advisory_path: AdvisoryPath::from_unix_bytes(path).unwrap(),
    })
}

fn register(
    store: &mut CheckpointStore,
    file_id: FileId,
    locator: Locator,
    offset: u64,
    fingerprint: &[u8],
    path: &[u8],
) {
    let _outcome = store
        .append(vec![registration(
            file_id,
            locator,
            offset,
            fingerprint,
            path,
        )])
        .unwrap();
}

fn no_live_locators() -> HashSet<Locator> {
    HashSet::new()
}

fn complete_inventory(
    candidates: &[CandidateEvidence],
    other_live_locators: &HashSet<Locator>,
    config: &IdentitySettings,
) -> CandidateInventory {
    CandidateInventory::from_complete_reconciliation(
        candidates,
        other_live_locators,
        config.fingerprint_bytes,
    )
}

fn resolve_and_persist(
    store: &mut CheckpointStore,
    candidates: &[CandidateEvidence],
    other_live_locators: &HashSet<Locator>,
    config: &IdentitySettings,
    now_unix_nano: u64,
) -> Result<Vec<ResolvedIdentity>, IdentityError> {
    let inventory = complete_inventory(candidates, other_live_locators, config);
    resolve_with_inventory(store, candidates, &inventory, config, now_unix_nano)
}

fn resolve_and_persist_with_source(
    store: &mut CheckpointStore,
    candidates: &[CandidateEvidence],
    other_live_locators: &HashSet<Locator>,
    config: &IdentitySettings,
    now_unix_nano: u64,
    file_ids: &mut impl FileIdSource,
) -> Result<Vec<ResolvedIdentity>, IdentityError> {
    let inventory = complete_inventory(candidates, other_live_locators, config);
    resolve_with_inventory_and_source(
        store,
        candidates,
        &inventory,
        config,
        now_unix_nano,
        file_ids,
    )
}

/// Scenario: an exact-locator candidate has a longer compatible fingerprint,
/// a changed advisory path, and a configured `start_at: end`.
/// Guarantees: durable checkpoint progress takes precedence, fingerprint and
/// metadata evidence grow atomically, and the opaque `file_id` never changes.
#[test]
fn exact_locator_resumes_checkpoint_and_grows_evidence_without_rekeying() {
    let (_directory, mut store, _options) = test_store(8);
    let file_id = FileId::from_bytes([1; 16]);
    register(&mut store, file_id, locator(1), 3, b"ab", b"old.log");
    let mut config = settings();
    config.start_at = StartAt::End;
    let candidate = evidence(locator(1), 20, b"abcd", b"new.log");

    let resolved =
        resolve_and_persist(&mut store, &[candidate], &no_live_locators(), &config, 9).unwrap();

    assert_eq!(resolved[0].file_id, file_id);
    assert_eq!(resolved[0].committed_offset, 3);
    assert_eq!(resolved[0].matched_by, IdentityMatch::ExactLocator);
    let record = store.table().get(&file_id).unwrap();
    assert_eq!(record.fingerprint, b"abcd");
    assert_eq!(
        record.advisory_path,
        AdvisoryPath::from_unix_bytes(b"new.log").unwrap()
    );
    assert_eq!(record.last_seen_time_unix_nano, 9);
}

/// Scenario: exact-locator checkpoints and candidates carry empty or short
/// matching evidence because their files have not reached a full window.
/// Guarantees: prefix matching handles both lengths without indexing past a
/// slice or refusing a stable handle identity.
#[test]
fn exact_locator_accepts_empty_and_short_fingerprints() {
    let (_directory, mut store, _options) = test_store(8);
    let empty_id = FileId::from_bytes([2; 16]);
    let short_id = FileId::from_bytes([3; 16]);
    register(&mut store, empty_id, locator(2), 0, b"", b"empty");
    register(&mut store, short_id, locator(3), 1, b"a", b"short");

    let resolved = resolve_and_persist(
        &mut store,
        &[
            evidence(locator(2), 0, b"", b"empty"),
            evidence(locator(3), 2, b"ab", b"short"),
        ],
        &no_live_locators(),
        &settings(),
        2,
    )
    .unwrap();

    assert_eq!(resolved[0].file_id, empty_id);
    assert_eq!(resolved[1].file_id, short_id);
    assert_eq!(store.table().get(&short_id).unwrap().fingerprint, b"ab");
}

/// Scenario: one complete fingerprint maps to one active checkpoint whose
/// old locator is absent, then the same setup is repeated while that old
/// locator is reported live.
/// Guarantees: fingerprint reconnect matches the identity and updates its
/// advisory path, but the locator is immutable for a `file_id` in this
/// version (`update_metadata` never carries one), so it is left exactly as
/// registered even though the candidate presents a different locator; a
/// live prior locator still prevents a second reader inheriting the
/// identity at all.
#[test]
fn fingerprint_reconnect_requires_previous_locator_to_be_absent() {
    let (_directory, mut store, _options) = test_store(8);
    let file_id = FileId::from_bytes([4; 16]);
    let old_locator = locator(4);
    register(&mut store, file_id, old_locator, 2, b"same", b"old");

    let resolved = resolve_and_persist(
        &mut store,
        &[evidence(locator(40), 9, b"same", b"moved")],
        &no_live_locators(),
        &settings(),
        3,
    )
    .unwrap();
    assert_eq!(resolved[0].file_id, file_id);
    assert_eq!(resolved[0].matched_by, IdentityMatch::UniqueFingerprint);
    // The locator is immutable for this `file_id`: it stays exactly as
    // registered, never adopting the reconnecting candidate's locator.
    assert_eq!(store.table().get(&file_id).unwrap().locator, old_locator);

    let (_directory, mut store, _options) = test_store(8);
    register(&mut store, file_id, old_locator, 2, b"same", b"old");
    let live = HashSet::from([old_locator]);
    let resolved = resolve_and_persist(
        &mut store,
        &[evidence(locator(41), 9, b"same", b"copy")],
        &live,
        &settings(),
        3,
    )
    .unwrap();
    assert_ne!(resolved[0].file_id, file_id);
    assert_eq!(resolved[0].matched_by, IdentityMatch::RecoveryMismatch);
}

/// Scenario: a brand-new file is registered at `start_at: beginning`
/// (`committed_offset == 0`).
/// Guarantees: the durable `committed_frontier_guard` is the exact empty
/// guard, never a fabricated placeholder computed from the offset alone.
#[test]
fn new_discovery_at_beginning_uses_the_exact_empty_guard() {
    let (_directory, mut store, _options) = test_store(8);
    let candidate = evidence_with_window(locator(20), 0, b"", b"new.log");

    let resolved = resolve_and_persist(
        &mut store,
        &[candidate],
        &no_live_locators(),
        &settings(),
        1,
    )
    .unwrap();

    assert_eq!(resolved[0].committed_offset, 0);
    let record = store.table().get(&resolved[0].file_id).unwrap();
    assert_eq!(
        record.committed_frontier_guard,
        CommittedFrontierGuard::empty()
    );
}

/// Scenario: a brand-new file is registered at `start_at: end`
/// (`committed_offset == candidate.size`), with real, non-zero-filled
/// committed-frontier window evidence read from the same validated handle.
/// Guarantees: the durable `committed_frontier_guard` is computed from
/// that exact real evidence -- not a fabricated all-zero placeholder for
/// the same offset, and not the empty guard.
#[test]
fn new_discovery_at_end_uses_the_real_captured_window() {
    let (_directory, mut store, _options) = test_store(8);
    let mut config = settings();
    config.start_at = StartAt::End;
    let candidate = evidence_with_window(locator(21), 100, b"abcd", b"new.log");
    let expected_guard = candidate.committed_frontier_window.guard().unwrap();

    let resolved =
        resolve_and_persist(&mut store, &[candidate], &no_live_locators(), &config, 1).unwrap();

    assert_eq!(resolved[0].committed_offset, 100);
    let record = store.table().get(&resolved[0].file_id).unwrap();
    assert_eq!(record.committed_frontier_guard, expected_guard);
    // A fabricated all-zero window for the same offset must not collide
    // with the real evidence's guard.
    let fabricated_guard = zero_guard(100);
    assert_ne!(record.committed_frontier_guard, fabricated_guard);
}

/// Scenario: two simultaneously live files have the same complete initial
/// bytes.
/// Guarantees: candidate-side ambiguity prevents fingerprint reconnect and
/// assigns distinct random durable identities to both files.
#[test]
fn identical_live_fingerprints_remain_distinct() {
    let (_directory, mut store, _options) = test_store(8);
    let candidates = [
        evidence(locator(5), 8, b"same", b"one"),
        evidence(locator(6), 8, b"same", b"two"),
    ];

    let resolved =
        resolve_and_persist(&mut store, &candidates, &no_live_locators(), &settings(), 4).unwrap();

    assert_ne!(resolved[0].file_id, resolved[1].file_id);
    assert_eq!(resolved[0].matched_by, IdentityMatch::NewDiscovery);
    assert_eq!(resolved[1].matched_by, IdentityMatch::NewDiscovery);
}

/// Scenario: two checkpoint records share a complete fingerprint and a
/// third locator presents those same bytes.
/// Guarantees: record-side ambiguity never inherits either checkpoint
/// offset and creates a new recovery-mismatch identity instead.
#[test]
fn duplicate_checkpoint_fingerprint_is_ambiguous() {
    let (_directory, mut store, _options) = test_store(8);
    let first_id = FileId::from_bytes([5; 16]);
    let second_id = FileId::from_bytes([6; 16]);
    register(&mut store, first_id, locator(50), 7, b"dupe", b"one");
    register(&mut store, second_id, locator(60), 8, b"dupe", b"two");

    let resolved = resolve_and_persist(
        &mut store,
        &[evidence(locator(70), 20, b"dupe", b"three")],
        &no_live_locators(),
        &settings(),
        5,
    )
    .unwrap();

    assert_ne!(resolved[0].file_id, first_id);
    assert_ne!(resolved[0].file_id, second_id);
    assert_eq!(resolved[0].committed_offset, 0);
    assert_eq!(resolved[0].matched_by, IdentityMatch::RecoveryMismatch);
}

/// Scenario: an exact locator reappears with changed fingerprint bytes or a
/// size below its committed offset.
/// Guarantees: neither invalid candidate inherits acknowledged progress;
/// each is handled as a fresh recovery-mismatch identity.
#[test]
fn exact_locator_mismatch_and_offset_beyond_size_never_resume() {
    let (_directory, mut store, _options) = test_store(8);
    let fingerprint_id = FileId::from_bytes([7; 16]);
    let offset_id = FileId::from_bytes([8; 16]);
    register(
        &mut store,
        fingerprint_id,
        locator(7),
        1,
        b"aaaa",
        b"fingerprint",
    );
    register(&mut store, offset_id, locator(8), 10, b"bbbb", b"offset");

    let resolved = resolve_and_persist(
        &mut store,
        &[
            evidence(locator(7), 20, b"zzzz", b"fingerprint"),
            evidence(locator(8), 9, b"bbbb", b"offset"),
        ],
        &no_live_locators(),
        &settings(),
        6,
    )
    .unwrap();

    assert_ne!(resolved[0].file_id, fingerprint_id);
    assert_ne!(resolved[1].file_id, offset_id);
    assert!(
        resolved
            .iter()
            .all(|identity| identity.matched_by == IdentityMatch::RecoveryMismatch)
    );
}

/// Scenario: an exact locator's current file is shorter than the fingerprint
/// evidence previously stored, although its remaining bytes are the same
/// prefix and its size still exceeds the committed offset.
/// Guarantees: durable evidence never shrinks and the candidate cannot
/// inherit an offset from a stream that was observably truncated or replaced.
#[test]
fn shorter_fingerprint_never_resumes_an_active_checkpoint() {
    let (_directory, mut store, _options) = test_store(8);
    let file_id = FileId::from_bytes([17; 16]);
    register(&mut store, file_id, locator(28), 2, b"abcd", b"shrink");

    let resolved = resolve_and_persist(
        &mut store,
        &[evidence(locator(28), 3, b"abc", b"shrink")],
        &no_live_locators(),
        &settings(),
        6,
    )
    .unwrap();

    assert_ne!(resolved[0].file_id, file_id);
    assert_eq!(resolved[0].committed_offset, 0);
    assert_eq!(resolved[0].matched_by, IdentityMatch::RecoveryMismatch);
}

/// Scenario: an otherwise valid exact locator has a different stored profile
/// version or digest and also presents different fingerprint bytes.
/// Guarantees: matching fails closed before any WAL append or metadata
/// mutation; a profile change cannot be disguised as a recovery mismatch.
#[test]
fn incompatible_resumption_profile_fails_closed() {
    let (_directory, mut store, _options) = test_store(8);
    let file_id = FileId::from_bytes([9; 16]);
    let mut operation = registration(file_id, locator(9), 0, b"abcd", b"profile");
    let Operation::RegisterFile(register) = &mut operation else {
        unreachable!()
    };
    register.framing_profile_digest = [0x99; 32];
    let _outcome = store.append(vec![operation]).unwrap();
    let before = store.stats();

    let error = resolve_and_persist(
        &mut store,
        &[evidence(locator(9), 4, b"zzzz", b"profile")],
        &no_live_locators(),
        &settings(),
        7,
    )
    .unwrap_err();

    assert!(matches!(error, IdentityError::IncompatibleProfile { .. }));
    assert_eq!(store.stats().wal_transactions, before.wal_transactions);
    assert_eq!(
        store
            .table()
            .get(&file_id)
            .unwrap()
            .last_seen_time_unix_nano,
        1
    );
}

/// Scenario: a namespace contains an active legacy profile-v1 record, but
/// the current reconciliation has no file candidates.
/// Guarantees: namespace compatibility is preflighted independently of
/// candidate matching and legacy resumable state fails closed without a WAL
/// mutation.
#[test]
fn legacy_profile_fails_closed_even_with_no_candidates() {
    let (_directory, mut store, _options) = test_store(8);
    let file_id = FileId::from_bytes([21; 16]);
    let mut operation = registration(file_id, locator(38), 0, b"abcd", b"legacy");
    let Operation::RegisterFile(register) = &mut operation else {
        unreachable!()
    };
    register.framing_profile_version = 1;
    register.framing_profile_digest =
        hex::decode("f00ca1eef473e3dc0dbd141e378270c0be2e6d698a4603d8bb63c81acbeed537")
            .unwrap()
            .try_into()
            .unwrap();
    let _outcome = store.append(vec![operation]).unwrap();
    let inventory = CandidateInventory::from_complete_reconciliation(
        &[],
        &no_live_locators(),
        settings().fingerprint_bytes,
    );
    let before = store.stats();

    let error = resolve_with_inventory(&mut store, &[], &inventory, &settings(), 7).unwrap_err();

    assert!(matches!(error, IdentityError::IncompatibleProfile { .. }));
    assert_eq!(store.stats().wal_transactions, before.wal_transactions);
}

/// Scenario: a durably quarantined locator reconnects, followed by a
/// replacement locator at the same advisory path.
/// Guarantees: the same locator preserves immutable quarantine identity,
/// while the replacement is a genuinely new discovery that does not inherit
/// quarantine or its offset.
#[test]
fn quarantine_reconnects_only_by_the_same_locator() {
    let (_directory, mut store, _options) = test_store(8);
    let file_id = FileId::from_bytes([10; 16]);
    register(&mut store, file_id, locator(10), 2, b"same", b"same-path");
    let _outcome = store
        .append(vec![Operation::QuarantineFile(QuarantineFile {
            file_id,
            expected_file_epoch: 1,
            reason_code: QUARANTINE_REASON_RECOVERY_MISMATCH,
            locator: locator(10),
            observed_size: 1,
            quarantine_epoch: 1,
            quarantine_time_unix_nano: 2,
        })])
        .unwrap();

    let same = resolve_and_persist(
        &mut store,
        &[evidence(locator(10), 4, b"same", b"same-path")],
        &no_live_locators(),
        &settings(),
        3,
    )
    .unwrap();
    assert_eq!(same[0].file_id, file_id);
    assert_eq!(same[0].lifecycle_state, LifecycleState::Quarantined);

    let changed = resolve_and_persist(
        &mut store,
        &[evidence(locator(10), 4, b"zzzz", b"same-path")],
        &no_live_locators(),
        &settings(),
        4,
    )
    .unwrap();
    assert_eq!(changed[0].file_id, file_id);
    assert_eq!(changed[0].lifecycle_state, LifecycleState::Quarantined);
    assert_eq!(store.table().get(&file_id).unwrap().fingerprint, b"same");

    let replacement = resolve_and_persist(
        &mut store,
        &[evidence(locator(11), 20, b"same", b"same-path")],
        &no_live_locators(),
        &settings(),
        5,
    )
    .unwrap();
    assert_ne!(replacement[0].file_id, file_id);
    assert_eq!(replacement[0].matched_by, IdentityMatch::NewDiscovery);
    assert_eq!(replacement[0].lifecycle_state, LifecycleState::Active);
}

/// Scenario: two discovered files with one complete fingerprint are split
/// across resolver batches, while the resolver receives a complete
/// reconciliation-wide multiplicity count.
/// Guarantees: the first batch cannot appear uniquely matchable merely
/// because the second candidate is processed later, so neither ordering can
/// assign an unrelated file the checkpoint's offset.
#[test]
fn fingerprint_uniqueness_is_reconciliation_wide_not_batch_local() {
    let (_directory, mut store, _options) = test_store(8);
    let file_id = FileId::from_bytes([18; 16]);
    register(&mut store, file_id, locator(29), 3, b"same", b"old");
    let candidate = evidence(locator(30), 10, b"same", b"first");
    let hidden_candidate = evidence(locator(31), 10, b"same", b"second");
    let inventory = CandidateInventory::from_complete_reconciliation(
        &[candidate.clone(), hidden_candidate],
        &no_live_locators(),
        settings().fingerprint_bytes,
    );

    let resolved =
        resolve_with_inventory(&mut store, &[candidate], &inventory, &settings(), 7).unwrap();

    assert_ne!(resolved[0].file_id, file_id);
    assert_eq!(resolved[0].matched_by, IdentityMatch::RecoveryMismatch);
    assert_eq!(resolved[0].committed_offset, 0);
}

/// Scenario: a reconciliation pass cannot prove that its fingerprint
/// inventory includes every pending candidate.
/// Guarantees: incomplete evidence disables fingerprint-only inheritance
/// rather than treating absence from the current batch as uniqueness.
#[test]
fn incomplete_candidate_inventory_disables_fingerprint_reconnect() {
    let (_directory, mut store, _options) = test_store(8);
    let file_id = FileId::from_bytes([19; 16]);
    register(&mut store, file_id, locator(32), 3, b"same", b"old");
    let candidate = evidence(locator(33), 10, b"same", b"first");
    let inventory = CandidateInventory::from_incomplete_reconciliation(
        std::slice::from_ref(&candidate),
        &no_live_locators(),
        settings().fingerprint_bytes,
    );

    let resolved =
        resolve_with_inventory(&mut store, &[candidate], &inventory, &settings(), 7).unwrap();

    assert_ne!(resolved[0].file_id, file_id);
    assert_eq!(resolved[0].matched_by, IdentityMatch::RecoveryMismatch);
}

/// Scenario: a caller incorrectly builds a "complete" inventory from only
/// one of two same-fingerprint candidates and then submits both candidates
/// in the resolver batch.
/// Guarantees: resolver-side multiplicity validation rejects the under-count
/// before either candidate can inherit or register durable state.
#[test]
fn complete_inventory_cannot_undercount_the_current_batch() {
    let (_directory, mut store, _options) = test_store(8);
    let candidates = [
        evidence(locator(36), 4, b"same", b"one"),
        evidence(locator(37), 4, b"same", b"two"),
    ];
    let inventory = CandidateInventory::from_complete_reconciliation(
        &candidates[..1],
        &HashSet::from([locator(37)]),
        settings().fingerprint_bytes,
    );

    let error =
        resolve_with_inventory(&mut store, &candidates, &inventory, &settings(), 7).unwrap_err();

    assert!(matches!(error, IdentityError::InvalidEvidence { .. }));
    assert!(store.table().is_empty());
}

/// Scenario: first discovery uses `start_at: end`, while unmatched recovery
/// evidence is configured for `skip_to_end` and `fail`.
/// Guarantees: the exact observed EOF anchor is durable before admission,
/// mismatch policy overrides `start_at`, and `fail` atomically registers a
/// quarantined record with bounded evidence.
#[test]
fn initial_offsets_and_recovery_mismatch_policies_are_durable() {
    let (_directory, mut store, options) = test_store(8);
    let mut config = settings();
    config.start_at = StartAt::End;
    let first = resolve_and_persist(
        &mut store,
        &[evidence(locator(12), 17, b"new!", b"first")],
        &no_live_locators(),
        &config,
        5,
    )
    .unwrap();
    assert_eq!(first[0].committed_offset, 17);
    let first_id = first[0].file_id;
    drop(store);
    let mut store = CheckpointStore::open(options).unwrap();
    assert_eq!(store.table().get(&first_id).unwrap().committed_offset, 17);

    let old_id = FileId::from_bytes([11; 16]);
    register(&mut store, old_id, locator(13), 4, b"old!", b"recover");
    config.on_recovery_mismatch = OnRecoveryMismatch::SkipToEnd;
    let skipped = resolve_and_persist(
        &mut store,
        &[evidence(locator(14), 23, b"diff", b"recover")],
        &no_live_locators(),
        &config,
        6,
    )
    .unwrap();
    assert_eq!(skipped[0].committed_offset, 23);
    assert_eq!(skipped[0].matched_by, IdentityMatch::RecoveryMismatch);

    let fail_old_id = FileId::from_bytes([12; 16]);
    register(&mut store, fail_old_id, locator(15), 4, b"old2", b"fail");
    config.on_recovery_mismatch = OnRecoveryMismatch::Fail;
    let failed = resolve_and_persist(
        &mut store,
        &[evidence(locator(16), 29, b"nope", b"fail")],
        &no_live_locators(),
        &config,
        7,
    )
    .unwrap();
    assert_eq!(failed[0].lifecycle_state, LifecycleState::Quarantined);
    let record = store.table().get(&failed[0].file_id).unwrap();
    assert_eq!(
        record.quarantine_evidence.as_ref().unwrap().reason_code,
        QUARANTINE_REASON_RECOVERY_MISMATCH
    );
}

#[derive(Debug)]
struct SequenceFileIds {
    values: Vec<FileId>,
    next: usize,
}

impl FileIdSource for SequenceFileIds {
    fn next_file_id(&mut self) -> FileId {
        let value = self.values[self.next];
        self.next += 1;
        value
    }
}

/// Scenario: random identity generation first collides with an existing
/// durable key and then returns a fresh value.
/// Guarantees: collision checking retries without overwriting the existing
/// record and persists only the fresh opaque identity.
#[test]
fn generated_file_id_is_collision_checked() {
    let (_directory, mut store, _options) = test_store(8);
    let existing = FileId::from_bytes([13; 16]);
    let fresh = FileId::from_bytes([14; 16]);
    register(&mut store, existing, locator(17), 0, b"old!", b"old");
    let mut source = SequenceFileIds {
        values: vec![existing, fresh],
        next: 0,
    };

    let resolved = resolve_and_persist_with_source(
        &mut store,
        &[evidence(locator(18), 4, b"new!", b"new")],
        &no_live_locators(),
        &settings(),
        8,
        &mut source,
    )
    .unwrap();

    assert_eq!(resolved[0].file_id, fresh);
    assert!(store.table().get(&existing).is_some());
    assert!(store.table().get(&fresh).is_some());
}

struct ConstantFileId(FileId);

impl FileIdSource for ConstantFileId {
    fn next_file_id(&mut self) -> FileId {
        self.0
    }
}

/// Scenario: the random source returns an already tracked `file_id` for
/// every bounded generation attempt.
/// Guarantees: admission reports a terminal entropy/collision error after a
/// fixed attempt count and leaves the durable table and WAL unchanged.
#[test]
fn repeated_file_id_collisions_are_bounded_and_non_mutating() {
    let (_directory, mut store, _options) = test_store(8);
    let existing = FileId::from_bytes([16; 16]);
    register(&mut store, existing, locator(26), 0, b"old!", b"old");
    let before = store.stats();
    let mut source = ConstantFileId(existing);

    let error = resolve_and_persist_with_source(
        &mut store,
        &[evidence(locator(27), 4, b"new!", b"new")],
        &no_live_locators(),
        &settings(),
        8,
        &mut source,
    )
    .unwrap_err();

    assert!(matches!(error, IdentityError::FileIdCollisionLimit { .. }));
    assert_eq!(store.stats().wal_transactions, before.wal_transactions);
    assert_eq!(store.table().len(), 1);
}

/// Scenario: a batch repeats a locator and another batch exceeds its
/// configured candidate bound.
/// Guarantees: both invalid batches fail before random ID generation or any
/// durable checkpoint mutation.
#[test]
fn candidate_batch_bounds_and_locator_uniqueness_are_enforced() {
    let (_directory, mut store, _options) = test_store(8);
    let duplicate = [
        evidence(locator(19), 1, b"a", b"one"),
        evidence(locator(19), 1, b"a", b"two"),
    ];
    assert!(matches!(
        resolve_and_persist(&mut store, &duplicate, &no_live_locators(), &settings(), 9,),
        Err(IdentityError::DuplicateCandidateLocator { .. })
    ));

    let mut config = settings();
    config.max_candidates = 1;
    assert!(matches!(
        resolve_and_persist(
            &mut store,
            &[
                evidence(locator(20), 1, b"a", b"one"),
                evidence(locator(21), 1, b"b", b"two"),
            ],
            &no_live_locators(),
            &config,
            9,
        ),
        Err(IdentityError::InvalidEvidence { .. })
    ));
    assert!(store.table().is_empty());
}

/// Scenario: several new candidates, including a recovery-mismatch `fail`
/// pair, are admitted in one bounded resolver call.
/// Guarantees: registrations are transaction-packed when possible, while
/// register-plus-quarantine remains one indivisible atomic group.
#[test]
fn batch_registration_packs_atomic_candidate_groups() {
    let (_directory, mut store, _options) = test_store(8);
    let old_id = FileId::from_bytes([15; 16]);
    register(&mut store, old_id, locator(22), 0, b"old!", b"mismatch");
    let before = store.stats().wal_transactions;
    let mut config = settings();
    config.on_recovery_mismatch = OnRecoveryMismatch::Fail;

    let resolved = resolve_and_persist(
        &mut store,
        &[
            evidence(locator(23), 4, b"new1", b"one"),
            evidence(locator(24), 4, b"new2", b"two"),
            evidence(locator(25), 4, b"diff", b"mismatch"),
        ],
        &no_live_locators(),
        &config,
        10,
    )
    .unwrap();

    assert_eq!(store.stats().wal_transactions, before + 1);
    assert_eq!(resolved[2].lifecycle_state, LifecycleState::Quarantined);
}

/// Scenario: every injected WAL write/sync failure interrupts an atomic
/// recovery-mismatch `register_file + quarantine_file` group.
/// Guarantees: reopening observes either no candidate record or one complete
/// quarantined record, never a register-only active identity that could read
/// around the configured `fail` policy.
#[test]
fn fail_policy_registration_is_atomic_across_wal_faults() {
    for point in FaultPoint::WAL_DURABILITY {
        let (_directory, mut initial, options) = test_store(8);
        let old_id = FileId::from_bytes([20; 16]);
        register(&mut initial, old_id, locator(34), 0, b"old!", b"mismatch");
        drop(initial);

        let mut faulted = CheckpointStore::open_with_fault(options.clone(), point).unwrap();
        let candidate = evidence(locator(35), 4, b"new!", b"mismatch");
        let mut config = settings();
        config.on_recovery_mismatch = OnRecoveryMismatch::Fail;
        assert!(
            resolve_and_persist(&mut faulted, &[candidate], &no_live_locators(), &config, 11,)
                .is_err(),
            "{point:?} must interrupt the append"
        );
        drop(faulted);

        let recovered = CheckpointStore::open(options).unwrap();
        let candidate_records: Vec<_> = recovered
            .table()
            .iter()
            .filter_map(|(_, record)| (record.locator == locator(35)).then_some(record))
            .collect();
        assert!(
            candidate_records.is_empty()
                || (candidate_records.len() == 1
                    && candidate_records[0].lifecycle_state == LifecycleState::Quarantined),
            "{point:?} recovered a register-only identity"
        );
    }
}

#[cfg(windows)]
/// Scenario: a real Windows handle-derived locator is registered, the
/// checkpoint namespace is closed and reopened, and the same file is opened
/// again.
/// Guarantees: Windows volume/file-ID evidence reconnects the durable
/// `file_id` and checkpoint offset across restart.
#[test]
fn windows_handle_identity_reconnects_after_checkpoint_reopen() {
    use super::platform::open_candidate;

    let (directory, mut store, options) = test_store(8);
    let path = directory.path().join("source.log");
    std::fs::write(&path, b"same").unwrap();
    let first_candidate = open_candidate(&path, false, 4, 0).unwrap();
    let first = resolve_and_persist(
        &mut store,
        &[first_candidate.evidence.clone()],
        &no_live_locators(),
        &settings(),
        12,
    )
    .unwrap();
    let file_id = first[0].file_id;
    drop(first_candidate);
    drop(store);

    let mut reopened_store = CheckpointStore::open(options).unwrap();
    let reopened_candidate = open_candidate(&path, false, 4, 0).unwrap();
    let reopened = resolve_and_persist(
        &mut reopened_store,
        &[reopened_candidate.evidence],
        &no_live_locators(),
        &settings(),
        13,
    )
    .unwrap();

    assert_eq!(reopened[0].file_id, file_id);
    assert_eq!(reopened[0].matched_by, IdentityMatch::ExactLocator);
}
