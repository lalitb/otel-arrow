// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use tempfile::TempDir;

use super::admission::AdmissionController;
use super::scanner::{
    DiscoveryPlan, FilesystemScanner, ReconciliationSchedule, validate_candidate_path_stability,
};
use super::source::{spawn_discovery, spawn_discovery_with_shutdown_signal};
use super::*;
use crate::receivers::filelog_receiver::checkpoint::primitives::{
    AdvisoryPath, CommittedFrontierGuard, CommittedFrontierWindow, FRAMING_PROFILE_VERSION, FileId,
    FramingResume,
};
use crate::receivers::filelog_receiver::checkpoint::store::{CheckpointStore, StoreOptions};
use crate::receivers::filelog_receiver::checkpoint::wal::{Operation, RegisterFile};
use crate::receivers::filelog_receiver::config::{Config, RuntimeConfig};
use crate::receivers::filelog_receiver::identity::matcher::{
    IdentityMatch, IdentitySettings, resolve_and_persist,
};

fn pattern(root: &Path, suffix: &str) -> String {
    root.join(suffix).to_string_lossy().into_owned()
}

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

fn zero_window(end_offset: u64) -> CommittedFrontierWindow {
    let window_len = end_offset.min(64) as usize;
    CommittedFrontierWindow::new(end_offset, vec![0u8; window_len]).unwrap()
}

#[cfg(unix)]
fn symlink_file(original: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(original, link)
}

#[cfg(windows)]
fn symlink_file(original: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(original, link)
}

#[cfg(unix)]
fn symlink_dir(original: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(original, link)
}

#[cfg(windows)]
fn symlink_dir(original: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(original, link)
}

fn runtime_config(root: &Path, include: Vec<String>, exclude: Vec<String>) -> RuntimeConfig {
    let mut config = Config {
        include,
        exclude,
        ..Config::default()
    };
    config.identity.fingerprint_bytes = 16;
    config.limits.max_tracked_files = 16;
    config.limits.max_pending_candidates = 8;
    config.limits.max_open_files = 4;
    config.discovery.reconcile_interval = Duration::from_secs(60);
    config.discovery.reconcile_jitter_percent = 0;
    let mut runtime = RuntimeConfig::from_config(config, "discovery-test").unwrap();
    runtime.checkpoint_namespace_dir = root.join(".checkpoint");
    runtime
}

fn scanner_and_admission(config: &RuntimeConfig) -> (FilesystemScanner, AdmissionController) {
    let plan = DiscoveryPlan::from_runtime(config).unwrap();
    let admission = AdmissionController::new(
        plan.max_pending_candidates(),
        plan.max_tracked_files(),
        plan.max_candidate_events(),
        plan.fingerprint_bytes(),
    )
    .unwrap();
    (FilesystemScanner::new(plan), admission)
}

/// Scenario: a 5-second reconciliation interval is sampled with zero and
/// 10-percent jitter at the deterministic lower, middle, and upper samples.
/// Guarantees: zero jitter is exact, nonzero jitter uses the inclusive
/// floor-based range, and selection never escapes the validated bounds.
#[test]
fn reconciliation_schedule_samples_exact_inclusive_bounds() {
    let exact = ReconciliationSchedule {
        minimum_delay_ns: 5_000_000_000,
        maximum_delay_ns: 5_000_000_000,
    };
    assert_eq!(
        exact.delay_for_sample(u64::MAX).unwrap(),
        Duration::from_secs(5)
    );

    let jittered = ReconciliationSchedule {
        minimum_delay_ns: 4_500_000_000,
        maximum_delay_ns: 5_500_000_000,
    };
    assert_eq!(
        jittered.delay_for_sample(0).unwrap(),
        Duration::from_millis(4_500)
    );
    assert_eq!(
        jittered.delay_for_sample(500_000_000).unwrap(),
        Duration::from_secs(5)
    );
    assert_eq!(
        jittered.delay_for_sample(1_000_000_000).unwrap(),
        Duration::from_millis(5_500)
    );
}

fn observed_candidates(batch: &ReconciliationBatch) -> Vec<&DiscoveredCandidate> {
    batch
        .events
        .iter()
        .filter_map(|event| match event {
            CandidateEvent::Observed(candidate) => Some(candidate),
            CandidateEvent::Updated(_)
            | CandidateEvent::Removed { .. }
            | CandidateEvent::Revoked { .. } => None,
        })
        .collect()
}

fn event_locators(batch: &ReconciliationBatch) -> Vec<Locator> {
    batch
        .events
        .iter()
        .filter_map(CandidateEvent::candidate)
        .map(|candidate| candidate.evidence.locator)
        .collect()
}

fn event_durable_acks(batch: &ReconciliationBatch) -> Vec<DurableAck> {
    batch
        .events
        .iter()
        .filter_map(CandidateEvent::candidate)
        .map(|candidate| DurableAck {
            locator: candidate.evidence.locator,
            advisory_path: candidate.evidence.advisory_path.clone(),
        })
        .collect()
}

fn fake_candidate(number: u64) -> DiscoveredCandidate {
    let path = PathBuf::from(format!("candidate-{number}.log"));
    DiscoveredCandidate {
        matched_path: path.clone(),
        resolved_path: path,
        evidence: CandidateEvidence {
            locator: Locator::PosixDevIno {
                dev: 1,
                ino: number,
            },
            size: 16,
            fingerprint: vec![number as u8; 16],
            advisory_path: AdvisoryPath::from_unix_bytes(
                format!("candidate-{number}.log").as_bytes(),
            )
            .unwrap(),
            committed_frontier_window: zero_window(16),
        },
        modified: None,
    }
}

/// Builds an alias observation for the same locator as `fake_candidate`,
/// naming it through a different literal path. Real overlapping globs and
/// hardlinks produce exactly this shape: one locator, several distinct
/// matched-path strings.
fn fake_candidate_alias(number: u64, alias: &str) -> DiscoveredCandidate {
    let mut candidate = fake_candidate(number);
    candidate.matched_path = PathBuf::from(alias);
    candidate.resolved_path = PathBuf::from(alias);
    candidate.evidence.advisory_path = AdvisoryPath::from_unix_bytes(alias.as_bytes()).unwrap();
    candidate
}

/// Scenario: a tracked locator, fingerprint, and advisory matched path stay
/// unchanged while the canonical target path changes.
/// Guarantees: discovery emits an update so an evicted logical reader can
/// reopen through the newly resolved target instead of retaining a stale
/// canonical path.
#[test]
fn resolved_path_change_emits_tracked_update() {
    let mut admission = AdmissionController::new(1, 1, 1, 16).unwrap();
    let original = fake_candidate(90);
    let locator = original.evidence.locator;

    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, original, Duration::ZERO)
        .unwrap();
    let first = admission.finish_scan().unwrap();
    admission.apply_feedback(durable_feedback(&first)).unwrap();

    let mut retargeted = fake_candidate(90);
    retargeted.resolved_path = PathBuf::from("retargeted-candidate-90.log");
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, retargeted, Duration::ZERO)
        .unwrap();
    let updated = admission.finish_scan().unwrap();

    assert!(matches!(
        updated.events.as_slice(),
        [CandidateEvent::Updated(candidate)]
            if candidate.evidence.locator == locator
                && candidate.resolved_path == Path::new("retargeted-candidate-90.log")
    ));
}

fn durable_feedback(batch: &ReconciliationBatch) -> DiscoveryFeedback {
    DiscoveryFeedback {
        durable: event_durable_acks(batch),
        ..DiscoveryFeedback::default()
    }
}

/// Scenario: overlapping recursive includes encounter root and nested log
/// files, an explicit excluded subtree, and checkpoint artifacts under the
/// broad include root.
/// Guarantees: includes are unioned, excludes win for matched paths, the
/// resolved checkpoint namespace is unconditional, and locator dedup emits
/// each eligible file exactly once.
#[test]
fn include_exclude_and_checkpoint_rules_are_enforced() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::write(root.join("root.log"), b"root").unwrap();
    std::fs::create_dir(root.join("nested")).unwrap();
    std::fs::write(root.join("nested/child.log"), b"child").unwrap();
    std::fs::create_dir(root.join("excluded")).unwrap();
    std::fs::write(root.join("excluded/secret.log"), b"secret").unwrap();
    std::fs::create_dir(root.join(".checkpoint")).unwrap();
    std::fs::write(root.join(".checkpoint/state.log"), b"state").unwrap();

    let config = runtime_config(
        root,
        vec![pattern(root, "**/*"), pattern(root, "root.log")],
        vec![pattern(root, "excluded")],
    );
    let plan = DiscoveryPlan::from_runtime(&config).unwrap();
    assert!(plan.likely_self_ingestion());
    let (mut scanner, mut admission) = scanner_and_admission(&config);

    let batch = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();
    let paths: HashSet<_> = observed_candidates(&batch)
        .into_iter()
        .map(|candidate| candidate.matched_path.clone())
        .collect();

    assert_eq!(
        paths,
        HashSet::from([root.join("root.log"), root.join("nested/child.log")])
    );
    assert!(batch.inventory.is_complete());
    assert_eq!(batch.stats.scan_errors, 0);
}

/// Scenario: a recursive glob can match a nested file, but the receiver's
/// independent `recursive` switch is disabled.
/// Guarantees: `**` does not override `recursive: false`; only files one
/// level below the include's literal root are discovered.
#[test]
fn recursive_false_limits_traversal_even_with_double_star() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::write(root.join("root.log"), b"root").unwrap();
    std::fs::create_dir(root.join("nested")).unwrap();
    std::fs::write(root.join("nested/child.log"), b"child").unwrap();
    let mut config = runtime_config(root, vec![pattern(root, "**/*.log")], vec![]);
    config.recursive = false;
    let (mut scanner, mut admission) = scanner_and_admission(&config);

    let batch = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();
    let observed = observed_candidates(&batch);

    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].matched_path, root.join("root.log"));
}

/// Scenario: two hardlink paths under overlapping glob traversal expose one
/// native runtime locator.
/// Guarantees: locator-level dedup retains one candidate and cannot create
/// parallel readers merely because path aliases differ.
#[test]
fn hardlinks_and_overlapping_globs_emit_one_locator() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let first = root.join("first.log");
    let second = root.join("second.log");
    std::fs::write(&first, b"same").unwrap();
    std::fs::hard_link(&first, &second).unwrap();
    let config = runtime_config(
        root,
        vec![pattern(root, "*.log"), pattern(root, "first.log")],
        vec![],
    );
    let (mut scanner, mut admission) = scanner_and_admission(&config);

    let batch = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();
    assert_eq!(observed_candidates(&batch).len(), 1);
}

#[cfg(any(unix, windows))]
/// Scenario: a matched file symlink targets an excluded path outside the
/// include root, first with link following disabled and then enabled.
/// Guarantees: no-follow mode rejects the final symlink, while follow mode
/// still applies excludes to the canonical target and cannot use a link to
/// bypass a sensitive-path exclusion.
#[test]
fn symlink_policy_and_resolved_target_excludes_are_enforced() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("secret.log");
    let link = root.path().join("linked.log");
    std::fs::write(&target, b"secret").unwrap();
    symlink_file(&target, &link).unwrap();

    let mut no_follow = runtime_config(root.path(), vec![pattern(root.path(), "*.log")], vec![]);
    no_follow.follow_symlinks = false;
    let (mut scanner, mut admission) = scanner_and_admission(&no_follow);
    let batch = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();
    assert!(observed_candidates(&batch).is_empty());

    let exact_no_follow = runtime_config(
        root.path(),
        vec![link.to_string_lossy().into_owned()],
        vec![],
    );
    let (mut scanner, mut admission) = scanner_and_admission(&exact_no_follow);
    let batch = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();
    assert!(observed_candidates(&batch).is_empty());

    let mut excluded_target = runtime_config(
        root.path(),
        vec![pattern(root.path(), "*.log")],
        vec![target.to_string_lossy().into_owned()],
    );
    excluded_target.follow_symlinks = true;
    let (mut scanner, mut admission) = scanner_and_admission(&excluded_target);
    let batch = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();
    assert!(observed_candidates(&batch).is_empty());

    let mut followed = runtime_config(root.path(), vec![pattern(root.path(), "*.log")], vec![]);
    followed.follow_symlinks = true;
    let (mut scanner, mut admission) = scanner_and_admission(&followed);
    let batch = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();
    assert_eq!(observed_candidates(&batch).len(), 1);
    assert_eq!(
        observed_candidates(&batch)[0].resolved_path,
        std::fs::canonicalize(target).unwrap()
    );
}

#[cfg(any(unix, windows))]
/// Scenario: discovery authorizes a canonical in-root candidate, then an
/// attacker replaces one ancestor with a directory link to an excluded
/// out-of-root file before the first candidate open.
/// Guarantees: the opened handle's native final path is compared with the
/// authorized target before fingerprint bytes are read, so the redirected
/// object is not admitted and the scan cannot provide absence evidence.
#[test]
fn ancestor_link_replacement_cannot_redirect_an_authorized_open() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let include_root = root.path().join("include");
    let candidate_dir = include_root.join("current");
    let parked_dir = root.path().join("parked");
    let excluded_dir = outside.path().join("excluded");
    std::fs::create_dir_all(&candidate_dir).unwrap();
    std::fs::create_dir(&excluded_dir).unwrap();
    std::fs::write(candidate_dir.join("app.log"), b"allowed").unwrap();
    std::fs::write(excluded_dir.join("app.log"), b"secret").unwrap();

    let config = runtime_config(
        root.path(),
        vec![pattern(&include_root, "**/*.log")],
        vec![pattern(&excluded_dir, "**")],
    );
    let (mut scanner, mut admission) = scanner_and_admission(&config);
    let gate = scanner.gate_next_candidate_before_first_open_for_test();
    let scan = std::thread::spawn(move || scanner.reconcile(&mut admission, SystemTime::now()));
    assert!(
        gate.wait_until_entered(Duration::from_secs(5)),
        "scanner did not reach the pre-open gate"
    );

    std::fs::rename(&candidate_dir, &parked_dir).unwrap();
    symlink_dir(&excluded_dir, &candidate_dir).unwrap();
    gate.release();

    let batch = scan.join().unwrap().unwrap();
    assert!(observed_candidates(&batch).is_empty());
    assert!(batch.stats.scan_errors >= 1);
    assert!(!batch.stats.complete);
    assert_eq!(
        std::fs::read(parked_dir.join("app.log")).unwrap(),
        b"allowed"
    );
}

#[cfg(any(unix, windows))]
/// Scenario: discovery observes a regular candidate beneath its canonical
/// traversal root, then an attacker replaces an ancestor with a directory
/// link to a different in-root file before candidate resolution.
/// Guarantees: no-follow discovery compares canonicalization with the exact
/// walked path, rejects the redirected object before opening it, and marks
/// the scan incomplete.
#[test]
fn ancestor_link_replacement_cannot_redirect_within_include_root() {
    let root = tempfile::tempdir().unwrap();
    let include_root = root.path().join("include");
    let candidate_dir = include_root.join("current");
    let parked_dir = root.path().join("parked");
    let redirected_dir = include_root.join("private");
    std::fs::create_dir_all(&candidate_dir).unwrap();
    std::fs::create_dir(&redirected_dir).unwrap();
    std::fs::write(candidate_dir.join("app.log"), b"allowed").unwrap();
    std::fs::write(redirected_dir.join("app.log"), b"secret").unwrap();

    let config = runtime_config(root.path(), vec![pattern(&candidate_dir, "*.log")], vec![]);
    let (mut scanner, mut admission) = scanner_and_admission(&config);
    let gate = scanner.gate_next_candidate_before_resolution_for_test();
    let scan = std::thread::spawn(move || scanner.reconcile(&mut admission, SystemTime::now()));
    assert!(
        gate.wait_until_entered(Duration::from_secs(5)),
        "scanner did not reach the pre-resolution gate"
    );

    std::fs::rename(&candidate_dir, &parked_dir).unwrap();
    symlink_dir(&redirected_dir, &candidate_dir).unwrap();
    gate.release();

    let batch = scan.join().unwrap().unwrap();
    assert!(observed_candidates(&batch).is_empty());
    assert!(batch.stats.scan_errors >= 1);
    assert!(!batch.stats.complete);
}

#[cfg(any(unix, windows))]
/// Scenario: an include's fixed literal directory prefix is itself a
/// symlink or Windows directory reparse alias, while descendant following
/// remains disabled.
/// Guarantees: the fixed traversal prefix is canonicalized and followed,
/// but the candidate retains its lexical matched path and canonical target.
#[test]
fn fixed_prefix_directory_alias_is_followed_in_no_follow_mode() {
    let root = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let target_file = target.path().join("app.log");
    let alias = root.path().join("alias");
    std::fs::write(&target_file, b"line").unwrap();
    symlink_dir(target.path(), &alias).unwrap();
    let config = runtime_config(root.path(), vec![pattern(&alias, "*.log")], vec![]);
    let (mut scanner, mut admission) = scanner_and_admission(&config);

    let batch = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();

    assert_eq!(observed_candidates(&batch).len(), 1);
    assert_eq!(
        observed_candidates(&batch)[0].matched_path,
        alias.join("app.log")
    );
    assert_eq!(
        observed_candidates(&batch)[0].resolved_path,
        std::fs::canonicalize(target_file).unwrap()
    );
}

/// Scenario: revalidating a candidate resolves to a different target, or a
/// no-follow candidate resolves outside its canonical traversal root.
/// Guarantees: both path-instability cases are explicit discovery issues,
/// allowing admission to suppress unproven removals for the pass.
#[test]
fn changed_candidate_resolution_is_not_treated_as_ordinary_exclusion() {
    let matched = Path::new("logs/app.log");
    let root = Path::new("/resolved/logs");
    let original = root.join("app.log");

    assert!(
        validate_candidate_path_stability(
            matched,
            &original,
            &root.join("replacement.log"),
            root,
            false,
        )
        .is_err()
    );
    let outside = Path::new("/resolved/outside/app.log");
    assert!(validate_candidate_path_stability(matched, outside, outside, root, false).is_err());
    assert!(validate_candidate_path_stability(matched, &original, &original, root, false,).is_ok());
}

#[cfg(any(unix, windows))]
/// Scenario: followed directory symlinks create a cycle back to the include
/// root.
/// Guarantees: traversal remains depth-bounded, reports the loop as an
/// incomplete pass, and still emits independently reachable regular files.
#[test]
fn followed_symlink_cycles_are_bounded_and_disable_complete_inventory() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::write(root.join("app.log"), b"line").unwrap();
    std::fs::create_dir(root.join("nested")).unwrap();
    symlink_dir(root, &root.join("nested/loop")).unwrap();
    let mut config = runtime_config(root, vec![pattern(root, "**/*.log")], vec![]);
    config.follow_symlinks = true;
    config.max_recursion_depth = 8;
    let (mut scanner, mut admission) = scanner_and_admission(&config);

    let batch = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();

    assert_eq!(observed_candidates(&batch).len(), 1);
    assert!(batch.stats.scan_errors >= 1);
    assert!(!batch.stats.complete);
    assert!(!batch.inventory.is_complete());
}

/// Scenario: one pending candidate survives an earlier pass before a second
/// candidate enters the bounded queue, and capacity later frees while scan
/// order is reversed.
/// Guarantees: retained pending candidates are admitted oldest-discovered
/// first rather than by current traversal order.
#[test]
fn retained_pending_candidates_are_admitted_oldest_first() {
    let mut admission = AdmissionController::new(2, 2, 1, 16).unwrap();
    let first = fake_candidate(1);
    let second = fake_candidate(2);

    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, first.clone(), Duration::ZERO)
        .unwrap();
    let batch = admission.finish_scan().unwrap();
    let first_locator = event_locators(&batch)[0];
    admission
        .apply_feedback(DiscoveryFeedback {
            deferred: vec![first_locator],
            ..DiscoveryFeedback::default()
        })
        .unwrap();
    assert_eq!(admission.pending_len(), 1);

    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, first.clone(), Duration::ZERO)
        .unwrap();
    admission
        .observe(generation, second.clone(), Duration::ZERO)
        .unwrap();
    let batch = admission.finish_scan().unwrap();
    assert_eq!(event_locators(&batch), vec![first_locator]);
    admission
        .apply_feedback(DiscoveryFeedback {
            deferred: vec![first_locator],
            ..DiscoveryFeedback::default()
        })
        .unwrap();

    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, second, Duration::ZERO)
        .unwrap();
    admission
        .observe(generation, first, Duration::ZERO)
        .unwrap();
    let batch = admission.finish_scan().unwrap();

    assert_eq!(
        event_locators(&batch),
        vec![fake_candidate(1).evidence.locator]
    );
    assert_eq!(batch.stats.pending_candidates, 1);
}

/// Scenario: identity defers a new candidate after the bounded pending
/// population has no free slot.
/// Guarantees: dropping that unretained evidence is carried into the next
/// reconciliation's overflow count and disables complete fingerprint
/// inventory claims instead of failing silently.
#[test]
fn deferred_candidate_overflow_is_reported_on_the_next_scan() {
    let mut admission = AdmissionController::new(0, 1, 1, 16).unwrap();
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, fake_candidate(1), Duration::ZERO)
        .unwrap();
    let observed = admission.finish_scan().unwrap();
    let locator = event_locators(&observed)[0];
    admission
        .apply_feedback(DiscoveryFeedback {
            deferred: vec![locator],
            ..DiscoveryFeedback::default()
        })
        .unwrap();

    let _ = admission.begin_scan(SystemTime::now()).unwrap();
    let next = admission.finish_scan().unwrap();

    assert_eq!(next.stats.overflowed_candidates, 1);
    assert!(!next.stats.complete);
    assert!(!next.inventory.is_complete());
}

/// Scenario: one retained candidate waits across a ten-second synthetic scan
/// interval before the event slot becomes available.
/// Guarantees: admission delay and pending age use explicit scan clocks and
/// require no sleeping or per-file telemetry state.
#[test]
fn retained_candidate_age_is_measured_deterministically() {
    let mut admission = AdmissionController::new(1, 2, 1, 16).unwrap();
    let start = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
    let monotonic_start = Instant::now();
    let generation = admission.begin_scan_at(start, monotonic_start).unwrap();
    admission
        .observe(generation, fake_candidate(1), Duration::ZERO)
        .unwrap();
    admission
        .observe(generation, fake_candidate(2), Duration::ZERO)
        .unwrap();
    let first = admission.finish_scan_at(monotonic_start).unwrap();
    assert_eq!(first.stats.pending_candidates, 1);
    assert_eq!(first.stats.oldest_pending_age, Duration::ZERO);
    admission
        .apply_feedback(DiscoveryFeedback {
            durable: vec![event_durable_acks(&first)[0].clone()],
            ..DiscoveryFeedback::default()
        })
        .unwrap();

    let later = start + Duration::from_secs(10);
    let generation = admission
        .begin_scan_at(later, monotonic_start + Duration::from_secs(10))
        .unwrap();
    let emitted_locator = event_locators(&first)[0];
    admission
        .observe(
            generation,
            if emitted_locator == fake_candidate(1).evidence.locator {
                fake_candidate(2)
            } else {
                fake_candidate(1)
            },
            Duration::ZERO,
        )
        .unwrap();
    let admitted = admission
        .finish_scan_at(monotonic_start + Duration::from_secs(10))
        .unwrap();
    assert_eq!(admitted.stats.admissions, 1);
    assert_eq!(admitted.stats.admission_delay, Duration::from_secs(10));
    assert_eq!(admitted.stats.pending_candidates, 0);
}

/// Scenario: reconciliation work completes five synthetic seconds after the
/// production finish path chooses its admission-decision timestamp.
/// Guarantees: scan duration, pending age, and overflow persistence use the
/// post-reconciliation completion clock rather than the earlier decision time.
#[test]
fn production_finish_clock_includes_reconciliation_work() {
    let mut admission = AdmissionController::new(1, 1, 1, 16).unwrap();
    let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
    let start = Instant::now();
    let generation = admission.begin_scan_at(wall, start).unwrap();
    for id in 1..=3 {
        admission
            .observe(generation, fake_candidate(id), Duration::ZERO)
            .unwrap();
    }
    let mut times = [
        start + Duration::from_secs(2),
        start + Duration::from_secs(7),
    ]
    .into_iter();

    let finished = admission
        .finish_scan_with_clock(&mut || times.next().expect("two finish clocks"))
        .unwrap();

    assert_eq!(finished.stats.scan_duration, Duration::from_secs(7));
    assert_eq!(finished.stats.oldest_pending_age, Duration::from_secs(7));
    assert_eq!(finished.stats.overflow_persistence, Duration::from_secs(7));
    assert!(times.next().is_none());
}

/// Scenario: a slow first overflowing scan is followed by continuous
/// overflow, one non-overflowing scan, and a later new overflow episode.
/// Guarantees: scan duration is included from the first scan start, overflow
/// persistence is continuous across scans, and a clear scan resets its age.
#[test]
fn overflow_persistence_includes_scan_time_and_resets() {
    let mut admission = AdmissionController::new(0, 8, 1, 16).unwrap();
    let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
    let start = Instant::now();

    let generation = admission.begin_scan_at(wall, start).unwrap();
    admission
        .observe(generation, fake_candidate(1), Duration::ZERO)
        .unwrap();
    admission
        .observe(generation, fake_candidate(2), Duration::ZERO)
        .unwrap();
    let first = admission
        .finish_scan_at(start + Duration::from_secs(7))
        .unwrap();
    assert_eq!(first.stats.scan_duration, Duration::from_secs(7));
    assert_eq!(first.stats.overflow_persistence, Duration::from_secs(7));
    admission
        .apply_feedback(DiscoveryFeedback {
            durable: event_durable_acks(&first),
            ..DiscoveryFeedback::default()
        })
        .unwrap();

    let generation = admission
        .begin_scan_at(wall, start + Duration::from_secs(10))
        .unwrap();
    admission
        .observe(generation, fake_candidate(3), Duration::ZERO)
        .unwrap();
    admission
        .observe(generation, fake_candidate(4), Duration::ZERO)
        .unwrap();
    let continuous = admission
        .finish_scan_at(start + Duration::from_secs(15))
        .unwrap();
    assert_eq!(
        continuous.stats.overflow_persistence,
        Duration::from_secs(15)
    );
    admission
        .apply_feedback(DiscoveryFeedback {
            durable: event_durable_acks(&continuous),
            ..DiscoveryFeedback::default()
        })
        .unwrap();

    let _generation = admission
        .begin_scan_at(wall, start + Duration::from_secs(16))
        .unwrap();
    let clear = admission
        .finish_scan_at(start + Duration::from_secs(17))
        .unwrap();
    assert_eq!(clear.stats.overflowed_candidates, 0);
    assert_eq!(clear.stats.overflow_persistence, Duration::ZERO);

    let generation = admission
        .begin_scan_at(wall, start + Duration::from_secs(20))
        .unwrap();
    admission
        .observe(generation, fake_candidate(5), Duration::ZERO)
        .unwrap();
    admission
        .observe(generation, fake_candidate(6), Duration::ZERO)
        .unwrap();
    let restarted = admission
        .finish_scan_at(start + Duration::from_secs(23))
        .unwrap();
    assert_eq!(restarted.stats.overflow_persistence, Duration::from_secs(3));
}

/// Scenario: four stable candidates repeatedly compete for one event slot
/// with no pending retention, and each admitted candidate is rejected so it
/// can compete again.
/// Guarantees: generation-keyed bounded selection varies opportunity rather
/// than permanently admitting the filesystem's first traversal entry.
#[test]
fn overflow_selection_varies_across_reconciliation_generations() {
    let mut admission = AdmissionController::new(0, 1, 1, 16).unwrap();
    let candidates: Vec<_> = (1..=4).map(fake_candidate).collect();
    let mut selected = HashSet::new();

    for _ in 0..128 {
        let generation = admission.begin_scan(SystemTime::now()).unwrap();
        for candidate in &candidates {
            admission
                .observe(generation, candidate.clone(), Duration::ZERO)
                .unwrap();
        }
        let batch = admission.finish_scan().unwrap();
        assert!(!batch.stats.complete);
        assert!(!batch.inventory.is_complete());
        let locator = event_locators(&batch)[0];
        let _ = selected.insert(locator);
        admission
            .apply_feedback(DiscoveryFeedback {
                rejected: vec![locator],
                ..DiscoveryFeedback::default()
            })
            .unwrap();
    }

    assert_eq!(selected.len(), candidates.len());
}

/// Scenario: a newly discovered candidate is older than the configured age
/// threshold, while an already tracked locator later presents the same old
/// modification time.
/// Guarantees: `ignore_older_than` filters only first admission and never
/// evicts or marks an already tracked file removed merely because it is quiet.
#[test]
fn ignore_older_than_applies_only_to_initial_admission() {
    let mut admission = AdmissionController::new(1, 1, 1, 16).unwrap();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
    let mut candidate = fake_candidate(1);
    let locator = candidate.evidence.locator;
    candidate.modified = Some(SystemTime::UNIX_EPOCH);

    let generation = admission.begin_scan(now).unwrap();
    admission
        .observe(generation, candidate.clone(), Duration::from_secs(10))
        .unwrap();
    let skipped = admission.finish_scan().unwrap();
    assert!(skipped.events.is_empty());
    assert_eq!(skipped.present_locators, HashSet::from([locator]));

    candidate.modified = Some(now);
    let generation = admission.begin_scan(now).unwrap();
    admission
        .observe(generation, candidate.clone(), Duration::from_secs(10))
        .unwrap();
    let admitted = admission.finish_scan().unwrap();
    assert_eq!(observed_candidates(&admitted).len(), 1);
    admission
        .apply_feedback(durable_feedback(&admitted))
        .unwrap();

    candidate.modified = Some(SystemTime::UNIX_EPOCH);
    let generation = admission.begin_scan(now).unwrap();
    admission
        .observe(generation, candidate, Duration::from_secs(10))
        .unwrap();
    let retained = admission.finish_scan().unwrap();
    assert!(retained.events.is_empty());
    assert_eq!(retained.present_locators, HashSet::from([locator]));
    assert_eq!(admission.tracked_locators().len(), 1);
}

/// Scenario: a previously unknown locator is observed only through an
/// excluded path during an otherwise complete reconciliation.
/// Guarantees: policy revocation still contributes bounded presence evidence,
/// so exclusion cannot be mistaken for continuous runtime absence.
#[test]
fn excluded_unknown_locator_remains_presence_evidence() {
    let mut admission = AdmissionController::new(1, 1, 1, 16).unwrap();
    let locator = Locator::PosixDevIno { dev: 8, ino: 81 };
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe_revoked(generation, locator, RevocationReason::ExcludedByPolicy)
        .unwrap();

    let batch = admission.finish_scan().unwrap();
    assert!(batch.stats.complete);
    assert_eq!(batch.present_locators, HashSet::from([locator]));
    assert!(batch.events.is_empty());
}

/// Scenario: a durable locator disappears, runtime retention removes its
/// checkpoint record, and the same locator is observed again later.
/// Guarantees: retention feedback drops discovery continuity state, so the
/// returning source is emitted as a fresh observation rather than an update
/// that could inherit the removed association.
#[test]
fn retention_feedback_forgets_removed_durable_locator() {
    let mut admission = AdmissionController::new(1, 1, 1, 16).unwrap();
    let candidate = fake_candidate(82);
    let locator = candidate.evidence.locator;

    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, candidate.clone(), Duration::ZERO)
        .unwrap();
    let first = admission.finish_scan().unwrap();
    admission.apply_feedback(durable_feedback(&first)).unwrap();

    let _generation = admission.begin_scan(SystemTime::now()).unwrap();
    let removed = admission.finish_scan().unwrap();
    assert!(matches!(
        removed.events.as_slice(),
        [CandidateEvent::Removed {
            locator: removed_locator
        }] if *removed_locator == locator
    ));
    admission
        .apply_feedback(DiscoveryFeedback {
            released: vec![DiscoveryRelease::RetentionRemoved(RetentionRemovalAck {
                locator,
                reconciliation_generation: removed.stats.generation,
            })],
            ..DiscoveryFeedback::default()
        })
        .unwrap();
    assert!(admission.tracked_locators().is_empty());

    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, candidate, Duration::ZERO)
        .unwrap();
    let returned = admission.finish_scan().unwrap();
    assert!(matches!(
        returned.events.as_slice(),
        [CandidateEvent::Observed(observed)] if observed.evidence.locator == locator
    ));
}

/// Scenario: one feedback transaction both acknowledges a candidate as
/// durable and claims retention removed the same locator.
/// Guarantees: contradictory duplicate ownership feedback is rejected before
/// either transition mutates discovery state.
#[test]
fn retention_feedback_cannot_duplicate_another_outcome() {
    let mut admission = AdmissionController::new(1, 1, 1, 16).unwrap();
    let candidate = fake_candidate(83);
    let locator = candidate.evidence.locator;
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, candidate, Duration::ZERO)
        .unwrap();
    let batch = admission.finish_scan().unwrap();
    let mut feedback = durable_feedback(&batch);
    feedback
        .released
        .push(DiscoveryRelease::RetentionRemoved(RetentionRemovalAck {
            locator,
            reconciliation_generation: batch.stats.generation,
        }));

    let error = admission.apply_feedback(feedback).unwrap_err();
    assert!(matches!(
        error,
        DiscoveryError::InvalidFeedback {
            locator: duplicate,
            reason: "one feedback transaction names the locator more than once"
        } if duplicate == locator
    ));
    assert_eq!(admission.tracked_locators(), HashSet::from([locator]));
}

/// Scenario: retention feedback from complete generation G arrives after
/// generation G+1 has already emitted a fresh observation for that locator.
/// Guarantees: stale cleanup is a no-op for the newer association, and its
/// ordinary durable acknowledgement remains valid.
#[test]
fn stale_retention_feedback_preserves_newer_observation() {
    let mut admission = AdmissionController::new(1, 1, 1, 16).unwrap();
    let _generation = admission.begin_scan(SystemTime::now()).unwrap();
    let absent = admission.finish_scan().unwrap();

    let candidate = fake_candidate(84);
    let locator = candidate.evidence.locator;
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, candidate, Duration::ZERO)
        .unwrap();
    let returned = admission.finish_scan().unwrap();
    assert!(matches!(
        returned.events.as_slice(),
        [CandidateEvent::Observed(observed)] if observed.evidence.locator == locator
    ));

    admission
        .apply_feedback(DiscoveryFeedback {
            released: vec![DiscoveryRelease::RetentionRemoved(RetentionRemovalAck {
                locator,
                reconciliation_generation: absent.stats.generation,
            })],
            ..DiscoveryFeedback::default()
        })
        .unwrap();
    assert_eq!(admission.tracked_locators(), HashSet::from([locator]));
    admission
        .apply_feedback(durable_feedback(&returned))
        .unwrap();
    assert_eq!(admission.tracked_locators(), HashSet::from([locator]));
}

/// Scenario: retention feedback claims absence from a reconciliation
/// generation discovery has not reached.
/// Guarantees: the internal protocol rejects future authority rather than
/// deleting current or pending continuity state.
#[test]
fn retention_feedback_rejects_future_generation() {
    let mut admission = AdmissionController::new(1, 1, 1, 16).unwrap();
    let _generation = admission.begin_scan(SystemTime::now()).unwrap();
    let batch = admission.finish_scan().unwrap();
    let locator = Locator::PosixDevIno { dev: 12, ino: 1 };

    let error = admission
        .apply_feedback(DiscoveryFeedback {
            released: vec![DiscoveryRelease::RetentionRemoved(RetentionRemovalAck {
                locator,
                reconciliation_generation: batch.stats.generation + 1,
            })],
            ..DiscoveryFeedback::default()
        })
        .unwrap_err();
    assert!(matches!(
        error,
        DiscoveryError::InvalidFeedback {
            locator: rejected,
            reason: "retention removal names a future reconciliation generation"
        } if rejected == locator
    ));
}

/// Scenario: more distinct excluded/ignored presence observations arrive
/// than the fixed tracked-plus-event evidence bound can retain.
/// Guarantees: the pass becomes incomplete instead of dropping a locator and
/// later treating that missing evidence as proof of absence.
#[test]
fn presence_evidence_overflow_marks_inventory_incomplete() {
    let mut admission = AdmissionController::new(1, 1, 1, 16).unwrap();
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    for ino in 1..=3 {
        admission
            .observe_revoked(
                generation,
                Locator::PosixDevIno { dev: 11, ino },
                RevocationReason::ExcludedByPolicy,
            )
            .unwrap();
    }

    let batch = admission.finish_scan().unwrap();
    assert!(!batch.stats.complete);
    assert_eq!(batch.present_locators.len(), 2);
}

/// Scenario: an admitted file disappears after durable feedback and remains
/// absent across another reconciliation before its reader finalizes.
/// Guarantees: `Removed` is emitted once, but the live locator remains in
/// identity inventories until explicit finalization closes the locator-reuse
/// window.
#[test]
fn removed_locator_stays_live_until_reader_finalization() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("app.log");
    std::fs::write(&path, b"line").unwrap();
    let config = runtime_config(
        directory.path(),
        vec![pattern(directory.path(), "*.log")],
        vec![],
    );
    let (mut scanner, mut admission) = scanner_and_admission(&config);

    let admitted = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();
    let locator = event_locators(&admitted)[0];
    admission
        .apply_feedback(durable_feedback(&admitted))
        .unwrap();
    std::fs::remove_file(path).unwrap();

    let removed = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();
    assert!(matches!(
        removed.events.as_slice(),
        [CandidateEvent::Removed { locator: found }] if *found == locator
    ));
    assert!(admission.tracked_locators().contains(&locator));

    let still_absent = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();
    assert!(still_absent.events.is_empty());
    admission
        .apply_feedback(DiscoveryFeedback {
            finalized: vec![locator],
            ..DiscoveryFeedback::default()
        })
        .unwrap();
    assert!(!admission.tracked_locators().contains(&locator));
}

/// Scenario: a durably admitted file is moved beneath a user-excluded
/// directory while retaining the same native locator, then moved back to an
/// eligible path after runtime acknowledges the revocation.
/// Guarantees: discovery emits positive `Revoked` evidence instead of
/// `Removed`, retains no excluded content evidence, and later emits `Updated`
/// for the same locator without creating a new identity.
#[test]
fn excluded_move_revokes_then_reeligibility_updates_same_locator() {
    let directory = tempfile::tempdir().unwrap();
    let allowed = directory.path().join("allowed");
    let denied = directory.path().join("denied");
    std::fs::create_dir(&allowed).unwrap();
    std::fs::create_dir(&denied).unwrap();
    let original = allowed.join("app.log");
    let excluded = denied.join("app.bin");
    std::fs::write(&original, b"line\n").unwrap();
    let config = runtime_config(
        directory.path(),
        vec![pattern(directory.path(), "**/*.log")],
        vec![pattern(&denied, "**")],
    );
    let (mut scanner, mut admission) = scanner_and_admission(&config);

    let admitted = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();
    let locator = event_locators(&admitted)[0];
    admission
        .apply_feedback(durable_feedback(&admitted))
        .unwrap();
    std::fs::rename(&original, &excluded).unwrap();

    let revoked = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();
    assert!(matches!(
        revoked.events.as_slice(),
        [CandidateEvent::Revoked {
            locator: found,
            reason: RevocationReason::ExcludedByPolicy,
        }] if *found == locator
    ));
    assert!(revoked.stats.complete);
    admission
        .apply_feedback(DiscoveryFeedback {
            released: vec![DiscoveryRelease::Revoked(locator)],
            ..DiscoveryFeedback::default()
        })
        .unwrap();

    let still_excluded = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();
    assert!(still_excluded.events.is_empty());
    std::fs::rename(&excluded, &original).unwrap();

    let reeligible = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();
    assert!(matches!(
        reeligible.events.as_slice(),
        [CandidateEvent::Updated(candidate)]
            if candidate.evidence.locator == locator
                && candidate.matched_path == original
    ));
}

/// Scenario: an active file remains eligible through its original path while
/// a hard link to the same native locator appears beneath an excluded path.
/// Guarantees: the excluded alias revokes the locator even though another
/// alias remains eligible, independent of traversal order.
#[test]
fn excluded_hard_link_revokes_an_eligible_alias() {
    let directory = tempfile::tempdir().unwrap();
    let allowed = directory.path().join("allowed");
    let denied = directory.path().join("denied");
    std::fs::create_dir(&allowed).unwrap();
    std::fs::create_dir(&denied).unwrap();
    let original = allowed.join("app.log");
    std::fs::write(&original, b"line\n").unwrap();
    let config = runtime_config(
        directory.path(),
        vec![pattern(directory.path(), "**/*.log")],
        vec![pattern(&denied, "**")],
    );
    let (mut scanner, mut admission) = scanner_and_admission(&config);
    let admitted = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();
    let locator = event_locators(&admitted)[0];
    admission
        .apply_feedback(durable_feedback(&admitted))
        .unwrap();

    std::fs::hard_link(&original, denied.join("alias.bin")).unwrap();
    let revoked = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();

    assert!(matches!(
        revoked.events.as_slice(),
        [CandidateEvent::Revoked {
            locator: found,
            reason: RevocationReason::ExcludedByPolicy,
        }] if *found == locator
    ));
}

/// Scenario: the first scan sees both an eligible path and an excluded hard
/// link for one previously unknown native locator.
/// Guarantees: per-scan denial suppresses initial admission regardless of
/// traversal order, so no content from the multiply named file is exported
/// before policy can revoke it.
#[test]
fn excluded_hard_link_suppresses_first_admission() {
    let directory = tempfile::tempdir().unwrap();
    let allowed = directory.path().join("allowed");
    let denied = directory.path().join("denied");
    std::fs::create_dir(&allowed).unwrap();
    std::fs::create_dir(&denied).unwrap();
    let original = allowed.join("app.log");
    std::fs::write(&original, b"secret\n").unwrap();
    std::fs::hard_link(&original, denied.join("alias.bin")).unwrap();
    let config = runtime_config(
        directory.path(),
        vec![pattern(directory.path(), "**/*.log")],
        vec![pattern(&denied, "**")],
    );
    let (mut scanner, mut admission) = scanner_and_admission(&config);

    let batch = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();

    assert!(batch.events.is_empty());
    assert!(admission.tracked_locators().is_empty());
}

/// Scenario: an include root and a disjoint exclude root contain hard links
/// to one previously unknown file, and the excluded alias does not match the
/// include extension.
/// Guarantees: the locator-only exclude-root pass runs before candidate
/// fingerprinting, so the external alias suppresses initial admission
/// without retaining excluded content evidence.
#[test]
fn external_excluded_hard_link_suppresses_first_admission() {
    let included = tempfile::tempdir().unwrap();
    let excluded = tempfile::tempdir().unwrap();
    let original = included.path().join("app.log");
    std::fs::write(&original, b"secret\n").unwrap();
    std::fs::hard_link(&original, excluded.path().join("alias.bin")).unwrap();
    let config = runtime_config(
        included.path(),
        vec![pattern(included.path(), "**/*.log")],
        vec![pattern(excluded.path(), "**")],
    );
    let (mut scanner, mut admission) = scanner_and_admission(&config);

    let batch = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();

    assert!(batch.events.is_empty());
    assert!(batch.stats.complete, "{:?}", batch.stats);
    assert!(admission.tracked_locators().is_empty());
}

/// Scenario: an eligible alias and an excluded alias name one already tracked
/// locator in each possible observation order.
/// Guarantees: positive exclusion wins deterministically, cancels any
/// same-generation update, and blocks re-eligibility until runtime
/// acknowledges the revocation.
#[test]
fn excluded_alias_wins_over_eligible_alias_in_either_order() {
    let mut admission = AdmissionController::new(1, 1, 1, 16).unwrap();
    let original = fake_candidate(7);
    let locator = original.evidence.locator;
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, original.clone(), Duration::ZERO)
        .unwrap();
    let admitted = admission.finish_scan().unwrap();
    admission
        .apply_feedback(durable_feedback(&admitted))
        .unwrap();

    let mut changed = original.clone();
    changed.matched_path = PathBuf::from("eligible-alias.log");
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, changed.clone(), Duration::ZERO)
        .unwrap();
    admission
        .observe_revoked(generation, locator, RevocationReason::ExcludedByPolicy)
        .unwrap();
    let eligible_first = admission.finish_scan().unwrap();
    assert!(matches!(
        eligible_first.events.as_slice(),
        [CandidateEvent::Revoked { locator: found, .. }] if *found == locator
    ));
    admission
        .apply_feedback(DiscoveryFeedback {
            released: vec![DiscoveryRelease::Revoked(locator)],
            ..DiscoveryFeedback::default()
        })
        .unwrap();

    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, changed.clone(), Duration::ZERO)
        .unwrap();
    let reeligible = admission.finish_scan().unwrap();
    assert!(matches!(
        reeligible.events.as_slice(),
        [CandidateEvent::Updated(_)]
    ));
    admission
        .apply_feedback(durable_feedback(&reeligible))
        .unwrap();

    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe_revoked(generation, locator, RevocationReason::ExcludedByPolicy)
        .unwrap();
    admission
        .observe(generation, changed, Duration::ZERO)
        .unwrap();
    let excluded_first = admission.finish_scan().unwrap();
    assert!(matches!(
        excluded_first.events.as_slice(),
        [CandidateEvent::Revoked { locator: found, .. }] if *found == locator
    ));
}

/// Scenario: a scan encounters arbitrarily many excluded locators that were
/// never retained by discovery.
/// Guarantees: unknown exclusion evidence consumes no tracked, pending, or
/// event capacity.
#[test]
fn unknown_excluded_locators_consume_no_admission_capacity() {
    let mut admission = AdmissionController::new(1, 1, 1, 16).unwrap();
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    for number in 1..=128 {
        admission
            .observe_revoked(
                generation,
                fake_candidate(number).evidence.locator,
                RevocationReason::ExcludedByPolicy,
            )
            .unwrap();
    }
    admission
        .observe(generation, fake_candidate(200), Duration::ZERO)
        .unwrap();
    let batch = admission.finish_scan().unwrap();

    assert!(batch.events.is_empty());
    assert!(!batch.stats.complete);
    assert!(admission.tracked_locators().is_empty());
    assert_eq!(admission.pending_len(), 0);
}

/// Scenario: positive exclusion arrives after an `Observed` event was handed
/// off but before worker feedback says whether the candidate became durable.
/// Guarantees: deferred admission followed by revocation feedback releases
/// the non-durable discovery entry, so later eligibility starts with a fresh
/// `Observed` transition rather than an invalid `Updated` transition.
#[test]
fn revocation_releases_a_candidate_deferred_before_durability() {
    let mut admission = AdmissionController::new(1, 1, 1, 16).unwrap();
    let candidate = fake_candidate(9);
    let locator = candidate.evidence.locator;
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, candidate.clone(), Duration::ZERO)
        .unwrap();
    let observed = admission.finish_scan().unwrap();
    assert!(matches!(
        observed.events.as_slice(),
        [CandidateEvent::Observed(_)]
    ));

    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe_revoked(generation, locator, RevocationReason::ExcludedByPolicy)
        .unwrap();
    let revoked = admission.finish_scan().unwrap();
    assert!(matches!(
        revoked.events.as_slice(),
        [CandidateEvent::Revoked { .. }]
    ));
    admission
        .apply_feedback(DiscoveryFeedback {
            deferred: vec![locator],
            ..DiscoveryFeedback::default()
        })
        .unwrap();
    admission
        .apply_feedback(DiscoveryFeedback {
            released: vec![DiscoveryRelease::Revoked(locator)],
            ..DiscoveryFeedback::default()
        })
        .unwrap();
    assert!(!admission.tracked_locators().contains(&locator));

    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, candidate, Duration::ZERO)
        .unwrap();
    let reeligible = admission.finish_scan().unwrap();
    assert!(matches!(
        reeligible.events.as_slice(),
        [CandidateEvent::Observed(_)]
    ));
}

/// Scenario: a traversal issue prevents a reconciliation pass from observing
/// either one durable locator or one retained pending candidate.
/// Guarantees: an incomplete traversal neither emits an unproven removal nor
/// evicts or admits stale pending evidence; the next complete absent pass
/// performs both transitions.
#[test]
fn incomplete_scan_preserves_unseen_tracked_and_pending_state() {
    let mut admission = AdmissionController::new(1, 2, 1, 16).unwrap();
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, fake_candidate(1), Duration::ZERO)
        .unwrap();
    admission
        .observe(generation, fake_candidate(2), Duration::ZERO)
        .unwrap();
    let initial = admission.finish_scan().unwrap();
    admission
        .apply_feedback(durable_feedback(&initial))
        .unwrap();
    assert_eq!(admission.tracked_locators().len(), 1);
    assert_eq!(admission.pending_len(), 1);

    let _ = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .record_issue(DiscoveryIssue::Io {
            operation: "test interrupted traversal",
            path: PathBuf::from("unreadable"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        })
        .unwrap();
    let incomplete = admission.finish_scan().unwrap();
    assert!(incomplete.events.is_empty());
    assert!(!incomplete.inventory.is_complete());
    assert_eq!(admission.tracked_locators().len(), 1);
    assert_eq!(admission.pending_len(), 1);

    let _ = admission.begin_scan(SystemTime::now()).unwrap();
    let complete = admission.finish_scan().unwrap();
    assert!(matches!(
        complete.events.as_slice(),
        [CandidateEvent::Removed { .. }]
    ));
    assert_eq!(admission.pending_len(), 0);
}

/// Scenario: a complete traversal proves one durable locator absent while
/// enough new candidates overflow the bounded event selection.
/// Guarantees: candidate overflow marks the inventory incomplete (batch
/// fingerprint-multiplicity validation stays conservative) but does not
/// suppress proven removal, and removals remain ordered after all observed
/// or updated transitions in the batch.
#[test]
fn candidate_overflow_still_emits_proven_removals_last() {
    let mut admission = AdmissionController::new(0, 2, 1, 16).unwrap();
    let original = fake_candidate(1);
    let original_locator = original.evidence.locator;
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, original, Duration::ZERO)
        .unwrap();
    let initial = admission.finish_scan().unwrap();
    admission
        .apply_feedback(durable_feedback(&initial))
        .unwrap();

    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, fake_candidate(2), Duration::ZERO)
        .unwrap();
    admission
        .observe(generation, fake_candidate(3), Duration::ZERO)
        .unwrap();
    let overflowed = admission.finish_scan().unwrap();

    assert!(!overflowed.stats.complete);
    assert!(matches!(
        overflowed.events.last(),
        Some(CandidateEvent::Removed { locator }) if *locator == original_locator
    ));
    assert!(matches!(
        overflowed.events.first(),
        Some(CandidateEvent::Observed(_))
    ));
}

/// Scenario: an observed transition is still in flight when its path
/// disappears, emits `Removed`, and reappears before durable feedback for the
/// original observation arrives.
/// Guarantees: blocked evidence does not silently mark the locator present;
/// the locator remains non-admissible through old-reader finalization, then a
/// later scan emits a fresh ordered `Observed` transition.
#[test]
fn reappearance_waits_for_reader_finalization_then_emits_observed() {
    let mut admission = AdmissionController::new(1, 1, 1, 16).unwrap();
    let candidate = fake_candidate(1);
    let locator = candidate.evidence.locator;

    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, candidate.clone(), Duration::ZERO)
        .unwrap();
    let observed = admission.finish_scan().unwrap();
    assert!(matches!(
        observed.events.as_slice(),
        [CandidateEvent::Observed(_)]
    ));

    let _removed = admission.begin_scan(SystemTime::now()).unwrap();
    let removed = admission.finish_scan().unwrap();
    assert!(matches!(
        removed.events.as_slice(),
        [CandidateEvent::Removed { .. }]
    ));

    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, candidate.clone(), Duration::ZERO)
        .unwrap();
    let blocked = admission.finish_scan().unwrap();
    assert!(blocked.events.is_empty());
    assert!(!blocked.stats.complete);
    admission
        .apply_feedback(DiscoveryFeedback {
            durable: vec![DurableAck {
                locator,
                advisory_path: candidate.evidence.advisory_path.clone(),
            }],
            ..DiscoveryFeedback::default()
        })
        .unwrap();

    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, candidate.clone(), Duration::ZERO)
        .unwrap();
    let still_blocked = admission.finish_scan().unwrap();
    assert!(still_blocked.events.is_empty());
    admission
        .apply_feedback(DiscoveryFeedback {
            finalized: vec![locator],
            ..DiscoveryFeedback::default()
        })
        .unwrap();

    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, candidate, Duration::ZERO)
        .unwrap();
    let reappeared = admission.finish_scan().unwrap();
    assert!(matches!(
        reappeared.events.as_slice(),
        [CandidateEvent::Observed(_)]
    ));
}

/// Scenario: a newly observed locator emits `Removed` before identity reports
/// that durable capacity deferred its original admission.
/// Guarantees: deferred feedback does not resurrect absent evidence in the
/// pending queue or discard the removal tombstone before ordered reader
/// finalization.
#[test]
fn deferred_removed_candidate_remains_a_tombstone_until_finalized() {
    let mut admission = AdmissionController::new(1, 1, 1, 16).unwrap();
    let candidate = fake_candidate(1);
    let locator = candidate.evidence.locator;
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, candidate, Duration::ZERO)
        .unwrap();
    let _ = admission.finish_scan().unwrap();
    let _ = admission.begin_scan(SystemTime::now()).unwrap();
    let removed = admission.finish_scan().unwrap();
    assert!(matches!(
        removed.events.as_slice(),
        [CandidateEvent::Removed { .. }]
    ));

    admission
        .apply_feedback(DiscoveryFeedback {
            deferred: vec![locator],
            ..DiscoveryFeedback::default()
        })
        .unwrap();
    assert!(admission.tracked_locators().contains(&locator));
    assert_eq!(admission.pending_len(), 0);

    admission
        .apply_feedback(DiscoveryFeedback {
            finalized: vec![locator],
            ..DiscoveryFeedback::default()
        })
        .unwrap();
    assert!(!admission.tracked_locators().contains(&locator));
}

/// Scenario: a durably admitted file is renamed within the include root
/// without changing its native locator or content prefix.
/// Guarantees: reconciliation emits `Updated` for the same locator and does
/// not create a second observed identity.
#[test]
fn rename_emits_updated_for_the_same_locator() {
    let directory = tempfile::tempdir().unwrap();
    let original = directory.path().join("app.log");
    let renamed = directory.path().join("renamed.log");
    std::fs::write(&original, b"line").unwrap();
    let config = runtime_config(
        directory.path(),
        vec![pattern(directory.path(), "*.log")],
        vec![],
    );
    let (mut scanner, mut admission) = scanner_and_admission(&config);

    let admitted = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();
    let locator = event_locators(&admitted)[0];
    admission
        .apply_feedback(durable_feedback(&admitted))
        .unwrap();
    std::fs::rename(original, &renamed).unwrap();

    let updated = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();

    assert!(matches!(
        updated.events.as_slice(),
        [CandidateEvent::Updated(candidate)]
            if candidate.evidence.locator == locator && candidate.matched_path == renamed
    ));
}

/// Scenario: the dedicated discovery thread performs its startup scan, then
/// receives durable feedback and an explicit scan request after a rename.
/// Guarantees: the bounded ordered source emits `Observed` before `Updated`,
/// processes feedback without shared mutable state, and separates
/// nonblocking cancellation from thread reaping.
#[test]
fn dedicated_source_orders_batches_and_shuts_down() {
    let directory = tempfile::tempdir().unwrap();
    let original = directory.path().join("app.log");
    let renamed = directory.path().join("renamed.log");
    std::fs::write(&original, b"line").unwrap();
    let config = runtime_config(
        directory.path(),
        vec![pattern(directory.path(), "*.log")],
        vec![],
    );
    let plan = DiscoveryPlan::from_runtime(&config).unwrap();
    let handle = spawn_discovery(plan).unwrap();

    let first = match handle.recv_timeout(Duration::from_secs(5)).unwrap() {
        DiscoveryMessage::Batch(batch) => *batch,
        other => panic!("expected initial discovery batch, got {other:?}"),
    };
    let locator = event_locators(&first)[0];
    handle.send_feedback(durable_feedback(&first)).unwrap();
    std::fs::rename(original, &renamed).unwrap();
    handle.scan_now().unwrap();

    let second = match handle.recv_timeout(Duration::from_secs(5)).unwrap() {
        DiscoveryMessage::Batch(batch) => *batch,
        other => panic!("expected requested discovery batch, got {other:?}"),
    };
    assert!(matches!(
        second.events.as_slice(),
        [CandidateEvent::Updated(candidate)]
            if candidate.evidence.locator == locator && candidate.matched_path == renamed
    ));
    handle.request_shutdown();
    assert!(matches!(
        handle.recv_timeout(Duration::from_secs(5)).unwrap(),
        DiscoveryMessage::Stopped
    ));
    handle
        .into_join_handle()
        .join()
        .map_err(|_| DiscoveryError::ThreadPanicked)
        .unwrap();
}

/// Scenario: the owning read worker remains unavailable while async teardown
/// asserts the cancellation signal shared directly with discovery.
/// Guarantees: discovery exits without requiring its command channel or the
/// read worker to resume first.
#[test]
fn shared_shutdown_signal_stops_discovery_out_of_band() {
    let directory = TempDir::new().unwrap();
    std::fs::write(directory.path().join("source.log"), b"line").unwrap();
    let config = runtime_config(
        directory.path(),
        vec![pattern(directory.path(), "*.log")],
        vec![],
    );
    let plan = DiscoveryPlan::from_runtime(&config).unwrap();
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let handle =
        spawn_discovery_with_shutdown_signal(plan, Arc::clone(&shutdown_requested)).unwrap();

    assert!(matches!(
        handle.recv_timeout(Duration::from_secs(5)).unwrap(),
        DiscoveryMessage::Batch(_)
    ));
    shutdown_requested.store(true, Ordering::Release);
    assert!(matches!(
        handle.recv_timeout(Duration::from_secs(5)).unwrap(),
        DiscoveryMessage::Stopped
    ));
    handle
        .into_join_handle()
        .join()
        .map_err(|_| DiscoveryError::ThreadPanicked)
        .unwrap();
}

/// Scenario: candidate sampling is gated after its first handle observation
/// while out-of-band discovery cancellation is asserted.
/// Guarantees: reconciliation stops before canonicalization, resampling, or
/// metadata lookup can begin and reports the explicit shutdown outcome.
#[test]
fn shared_shutdown_signal_stops_mid_candidate_sampling() {
    let directory = TempDir::new().unwrap();
    std::fs::write(directory.path().join("source.log"), b"line").unwrap();
    let config = runtime_config(
        directory.path(),
        vec![pattern(directory.path(), "*.log")],
        vec![],
    );
    let plan = DiscoveryPlan::from_runtime(&config).unwrap();
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let mut scanner =
        FilesystemScanner::with_shutdown_signal(plan.clone(), Arc::clone(&shutdown_requested));
    let gate = scanner.gate_next_candidate_after_first_sample_for_test();
    let mut admission = AdmissionController::new(
        plan.max_pending_candidates(),
        plan.max_tracked_files(),
        plan.max_candidate_events(),
        plan.fingerprint_bytes(),
    )
    .unwrap();
    let reconciliation =
        std::thread::spawn(move || scanner.reconcile(&mut admission, SystemTime::now()));

    assert!(
        gate.wait_until_entered(Duration::from_secs(5)),
        "scanner did not reach the gated candidate sample"
    );
    shutdown_requested.store(true, Ordering::Release);
    gate.release();

    assert!(matches!(
        reconciliation.join().unwrap(),
        Err(DiscoveryError::ShutdownRequested)
    ));
}

/// Scenario: an exact literal include names one regular file rather than a
/// directory root.
/// Guarantees: depth-zero file roots are evaluated and admitted instead of
/// being mistaken for traversal-only directories.
#[test]
fn literal_file_include_is_discovered() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("literal.log");
    std::fs::write(&path, b"line").unwrap();
    let config = runtime_config(
        directory.path(),
        vec![path.to_string_lossy().into_owned()],
        vec![],
    );
    let (mut scanner, mut admission) = scanner_and_admission(&config);

    let batch = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();

    assert_eq!(observed_candidates(&batch).len(), 1);
    assert_eq!(observed_candidates(&batch)[0].matched_path, path);
}

/// Scenario: a separator-free relative glob scans the current directory
/// without a literal path prefix.
/// Guarantees: the synthetic traversal root does not add `./` to the path
/// presented to the compiled matcher or to advisory matched-path evidence.
#[test]
fn relative_glob_uses_relative_matched_paths() {
    let file = tempfile::Builder::new()
        .prefix(".filelog-relative-")
        .suffix(".log")
        .tempfile_in(".")
        .unwrap();
    file.as_file().set_len(4).unwrap();
    let relative_path = PathBuf::from(file.path().file_name().unwrap());
    for include in ["*.log", "./*.log"] {
        let mut config = runtime_config(Path::new("."), vec![include.to_owned()], vec![]);
        config.recursive = false;
        let (mut scanner, mut admission) = scanner_and_admission(&config);

        let batch = scanner
            .reconcile(&mut admission, SystemTime::now())
            .unwrap();

        assert!(
            observed_candidates(&batch)
                .iter()
                .any(|candidate| candidate.matched_path == relative_path),
            "relative include did not match: {include}"
        );
    }
}

/// Scenario: lifecycle cancellation is already set when a reconciliation
/// pass would otherwise begin filesystem traversal.
/// Guarantees: discovery exits with its explicit shutdown result before
/// creating partial admission state or touching the filesystem.
#[test]
fn scanner_honors_cooperative_shutdown_before_reconciliation() {
    let directory = TempDir::new().unwrap();
    let config = runtime_config(
        directory.path(),
        vec![pattern(directory.path(), "**/*.log")],
        vec![],
    );
    let plan = DiscoveryPlan::from_runtime(&config).unwrap();
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let mut scanner =
        FilesystemScanner::with_shutdown_signal(plan.clone(), Arc::clone(&shutdown_requested));
    let mut admission = AdmissionController::new(
        plan.max_pending_candidates(),
        plan.max_tracked_files(),
        plan.max_candidate_events(),
        plan.fingerprint_bytes(),
    )
    .unwrap();
    shutdown_requested.store(true, Ordering::Release);

    assert!(matches!(
        scanner.reconcile(&mut admission, SystemTime::now()),
        Err(DiscoveryError::ShutdownRequested)
    ));
}

#[cfg(unix)]
/// Scenario: an exact Unix include escapes bracket, star, and backslash
/// characters that are literal bytes in the filename.
/// Guarantees: traversal uses the unescaped filesystem path while the
/// compiled glob retains its escapes and discovers the literal file.
#[test]
fn escaped_glob_metacharacters_form_a_literal_traversal_root() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join(r"app[1]*\name.log");
    std::fs::write(&path, b"line").unwrap();
    let include = format!(
        r"{}/app\[1\]\*\\name.log",
        directory.path().to_string_lossy()
    );
    let config = runtime_config(directory.path(), vec![include], vec![]);
    let (mut scanner, mut admission) = scanner_and_admission(&config);

    let batch = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();

    assert_eq!(observed_candidates(&batch).len(), 1);
    assert_eq!(observed_candidates(&batch)[0].matched_path, path);
}

/// Scenario: the durable table has no free record slots, but a discovered
/// candidate exactly matches the locator and prefix of its existing record.
/// Guarantees: zero new-identity capacity does not block recovery probes;
/// identity resolution reconnects the existing `file_id` without attempting
/// another registration.
#[test]
fn full_tracked_table_still_recovers_existing_identity() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("existing.log");
    std::fs::write(&path, b"existing").unwrap();
    let config = runtime_config(
        directory.path(),
        vec![pattern(directory.path(), "*.log")],
        vec![],
    );
    let (mut scanner, mut admission) = scanner_and_admission(&config);
    let batch = scanner
        .reconcile(&mut admission, SystemTime::now())
        .unwrap();
    let candidate = observed_candidates(&batch)[0].evidence.clone();

    let checkpoint = tempfile::tempdir().unwrap();
    let mut options = StoreOptions::new(checkpoint.path().join("state"), "full".to_owned());
    options.max_tracked_files = 1;
    options.fingerprint_bytes = 16;
    let mut store = CheckpointStore::open(options).unwrap();
    let file_id = FileId::from_bytes([0x55; 16]);
    let _ = store
        .append(vec![Operation::RegisterFile(RegisterFile {
            file_id,
            file_epoch: 1,
            committed_offset: 2,
            committed_frontier_guard: zero_guard(2),
            fingerprint: candidate.fingerprint.clone(),
            ignored_header_bytes: 0,
            locator: candidate.locator,
            framing_profile_version: FRAMING_PROFILE_VERSION,
            framing_profile_digest: config.framing_profile_digest,
            framing_resume: FramingResume::Clean,
            last_seen_time_unix_nano: 1,
            advisory_path: candidate.advisory_path.clone(),
        })])
        .unwrap();
    let settings = IdentitySettings::from_runtime(&config);

    let resolved =
        resolve_and_persist(&mut store, &[candidate], &batch.inventory, &settings, 2).unwrap();

    assert_eq!(resolved[0].file_id, file_id);
    assert_eq!(resolved[0].matched_by, IdentityMatch::ExactLocator);
    assert_eq!(store.table().len(), 1);
}

/// Scenario: one previously untracked locator has two simultaneously
/// eligible aliases, observed in each possible order across two otherwise
/// identical reconciliation passes.
/// Guarantees: both passes select the same deterministic-minimum
/// `(path_kind, complete native path bytes)` alias as the distinguished
/// binding, independent of traversal order, and neither retains the other
/// alias.
#[test]
fn deterministic_min_selection_independent_of_traversal_order() {
    let smaller = fake_candidate_alias(200, "aaa.log");
    let larger = fake_candidate_alias(200, "zzz.log");

    let mut forward = AdmissionController::new(4, 4, 4, 16).unwrap();
    let generation = forward.begin_scan(SystemTime::now()).unwrap();
    forward
        .observe(generation, larger.clone(), Duration::ZERO)
        .unwrap();
    forward
        .observe(generation, smaller.clone(), Duration::ZERO)
        .unwrap();
    let forward_batch = forward.finish_scan().unwrap();

    let mut reverse = AdmissionController::new(4, 4, 4, 16).unwrap();
    let generation = reverse.begin_scan(SystemTime::now()).unwrap();
    reverse
        .observe(generation, smaller.clone(), Duration::ZERO)
        .unwrap();
    reverse.observe(generation, larger, Duration::ZERO).unwrap();
    let reverse_batch = reverse.finish_scan().unwrap();

    let forward_candidates = observed_candidates(&forward_batch);
    let reverse_candidates = observed_candidates(&reverse_batch);
    assert_eq!(forward_candidates.len(), 1);
    assert_eq!(reverse_candidates.len(), 1);
    assert_eq!(forward_candidates[0].matched_path, PathBuf::from("aaa.log"));
    assert_eq!(reverse_candidates[0].matched_path, PathBuf::from("aaa.log"));
}

/// Scenario: a locator's distinguished binding is already stable, and a
/// later pass simultaneously observes that same path plus a newly
/// appearing, lexically smaller alias for the same locator.
/// Guarantees: the existing binding remains stable -- no false rebind --
/// even though a smaller alias is now present.
#[test]
fn lexically_smaller_alias_does_not_replace_stable_binding() {
    let mut admission = AdmissionController::new(4, 4, 4, 16).unwrap();
    let initial = fake_candidate_alias(201, "zzz.log");
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, initial.clone(), Duration::ZERO)
        .unwrap();
    let first = admission.finish_scan().unwrap();
    admission.apply_feedback(durable_feedback(&first)).unwrap();

    let smaller_alias = fake_candidate_alias(201, "aaa.log");
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, initial, Duration::ZERO)
        .unwrap();
    admission
        .observe(generation, smaller_alias, Duration::ZERO)
        .unwrap();
    let second = admission.finish_scan().unwrap();

    assert!(second.stats.complete);
    for event in &second.events {
        if let CandidateEvent::Updated(candidate) = event {
            assert_eq!(candidate.matched_path, PathBuf::from("zzz.log"));
        }
    }
}

/// Scenario: a locator's distinguished path is no longer observed, a
/// different alias for the same locator would otherwise become its new
/// minimum, but the pass is separately proven incomplete.
/// Guarantees: an incomplete pass never reselects a distinguished binding
/// from partial evidence; the old binding is preserved and no `Updated` or
/// `Removed` transition is emitted for it.
#[test]
fn incomplete_pass_preserves_binding_instead_of_reselecting() {
    let mut admission = AdmissionController::new(4, 4, 4, 16).unwrap();
    let initial = fake_candidate_alias(202, "old.log");
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, initial, Duration::ZERO)
        .unwrap();
    let first = admission.finish_scan().unwrap();
    admission.apply_feedback(durable_feedback(&first)).unwrap();

    let replacement_alias = fake_candidate_alias(202, "new.log");
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, replacement_alias, Duration::ZERO)
        .unwrap();
    admission
        .record_issue(DiscoveryIssue::Io {
            operation: "test forced incompleteness",
            path: PathBuf::from("unrelated"),
            source: std::io::Error::other("boom"),
        })
        .unwrap();
    let second = admission.finish_scan().unwrap();

    assert!(!second.stats.complete);
    assert!(second.events.is_empty());
}

/// Scenario: two different tracked locators' frozen distinguished paths are
/// both observed this pass naming the same new locator (for example, two
/// hardlinks of one new file each landing on a different owner's prior
/// path).
/// Guarantees: the conflicting claim is refused for both owners, their old
/// bindings are preserved, no `Updated` transition reassigns either path,
/// and the claimant is not recognized as anyone's replacement.
#[test]
fn conflicting_prior_bindings_fail_closed_and_preserve_old_bindings() {
    let mut admission = AdmissionController::new(8, 8, 8, 16).unwrap();
    let owner_a = fake_candidate_alias(203, "shared.log");
    let owner_b = fake_candidate_alias(204, "other.log");
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, owner_a, Duration::ZERO)
        .unwrap();
    admission
        .observe(generation, owner_b, Duration::ZERO)
        .unwrap();
    let first = admission.finish_scan().unwrap();
    admission.apply_feedback(durable_feedback(&first)).unwrap();

    let claimant_at_a = fake_candidate_alias(205, "shared.log");
    let claimant_at_b = fake_candidate_alias(205, "other.log");
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, claimant_at_a, Duration::ZERO)
        .unwrap();
    admission
        .observe(generation, claimant_at_b, Duration::ZERO)
        .unwrap();
    let second = admission.finish_scan().unwrap();

    assert!(!second.stats.complete);
    assert!(second.recognized_replacements.is_empty());
    for event in &second.events {
        assert!(!matches!(event, CandidateEvent::Updated(_)));
    }
}

/// Scenario: two durable records enter a scan with the same distinguished
/// binding, as can occur when reopening checkpoint state written before
/// binding uniqueness was enforced.
/// Guarantees: the duplicate frozen path has no arbitrary owner, the scan is
/// incomplete, and a locator observed at that path is not recognized as
/// either record's replacement.
#[test]
fn duplicate_durable_bindings_are_ambiguous_and_cannot_recognize_replacement() {
    let mut admission = AdmissionController::new(8, 8, 8, 16).unwrap();
    let owner_a = fake_candidate_alias(220, "a.log");
    let owner_b = fake_candidate_alias(221, "b.log");
    let locator_a = owner_a.evidence.locator;
    let locator_b = owner_b.evidence.locator;
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, owner_a, Duration::ZERO)
        .unwrap();
    admission
        .observe(generation, owner_b, Duration::ZERO)
        .unwrap();
    let first = admission.finish_scan().unwrap();
    let duplicate_path = AdvisoryPath::from_unix_bytes(b"shared.log").unwrap();
    admission
        .apply_feedback(DiscoveryFeedback {
            durable: vec![
                DurableAck {
                    locator: locator_a,
                    advisory_path: duplicate_path.clone(),
                },
                DurableAck {
                    locator: locator_b,
                    advisory_path: duplicate_path,
                },
            ],
            ..DiscoveryFeedback::default()
        })
        .unwrap();
    assert_eq!(first.events.len(), 2);

    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(
            generation,
            fake_candidate_alias(222, "shared.log"),
            Duration::ZERO,
        )
        .unwrap();
    let second = admission.finish_scan().unwrap();

    assert!(!second.stats.complete);
    assert!(second.recognized_replacements.is_empty());
    assert!(second.events.iter().all(|event| !matches!(
        event,
        CandidateEvent::Updated(candidate)
            if candidate.evidence.locator == locator_a
                || candidate.evidence.locator == locator_b
    )));
}

/// Scenario: an `Active` locator's distinguished binding is excluded
/// (revoked) and later becomes included again through its same exact path,
/// simultaneously with a newly appearing, lexically smaller alias.
/// Guarantees: revocation preserves the durable binding bit-for-bit, and
/// re-eligibility resumes it unchanged rather than reselecting merely
/// because a smaller alias is now also present.
#[test]
fn revocation_preserves_binding_and_reinclusion_does_not_reselect() {
    let mut admission = AdmissionController::new(4, 4, 4, 16).unwrap();
    let candidate = fake_candidate_alias(206, "kept.log");
    let locator = candidate.evidence.locator;

    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, candidate.clone(), Duration::ZERO)
        .unwrap();
    let first = admission.finish_scan().unwrap();
    admission.apply_feedback(durable_feedback(&first)).unwrap();

    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe_revoked(generation, locator, RevocationReason::ExcludedByPolicy)
        .unwrap();
    let revoked = admission.finish_scan().unwrap();
    assert!(matches!(
        revoked.events.as_slice(),
        [CandidateEvent::Revoked { locator: revoked_locator, .. }] if *revoked_locator == locator
    ));
    admission
        .apply_feedback(DiscoveryFeedback {
            released: vec![DiscoveryRelease::Revoked(locator)],
            ..DiscoveryFeedback::default()
        })
        .unwrap();

    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, candidate, Duration::ZERO)
        .unwrap();
    admission
        .observe(
            generation,
            fake_candidate_alias(206, "aaa-smaller.log"),
            Duration::ZERO,
        )
        .unwrap();
    let reincluded = admission.finish_scan().unwrap();

    let updated: Vec<_> = reincluded
        .events
        .iter()
        .filter_map(|event| match event {
            CandidateEvent::Updated(candidate) => Some(candidate),
            _ => None,
        })
        .collect();
    assert_eq!(updated.len(), 1);
    assert_eq!(updated[0].matched_path, PathBuf::from("kept.log"));
}

/// Scenario: one tracked locator's frozen distinguished path is observed
/// this pass naming two different newly appearing locators at once (an
/// unstable replacement -- for example, two racing observations of the
/// same path before either supersedes the other).
/// Guarantees: the second differing claimant poisons the claim for this
/// owner: neither claimant is recognized as its replacement, the owner's
/// old binding is preserved (no `Updated` transition reassigns it, and a
/// later pass observing the owner unchanged at its own path still finds it
/// unmodified), and the pass is reported incomplete.
#[test]
fn unstable_replacement_with_two_claimants_for_one_owner_poisons_the_claim() {
    let mut admission = AdmissionController::new(8, 8, 8, 16).unwrap();
    let owner = fake_candidate_alias(207, "owned.log");
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, owner, Duration::ZERO)
        .unwrap();
    let first = admission.finish_scan().unwrap();
    admission.apply_feedback(durable_feedback(&first)).unwrap();

    let claimant_one = fake_candidate_alias(208, "owned.log");
    let claimant_two = fake_candidate_alias(209, "owned.log");
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, claimant_one, Duration::ZERO)
        .unwrap();
    admission
        .observe(generation, claimant_two, Duration::ZERO)
        .unwrap();
    let second = admission.finish_scan().unwrap();

    assert!(!second.stats.complete);
    assert!(second.recognized_replacements.is_empty());
    for event in &second.events {
        assert!(!matches!(event, CandidateEvent::Updated(_)));
    }

    // The owner's binding survived the conflicting pass unmodified: a
    // later pass observing it, still unchanged, at its own original path
    // reports a complete scan with no reassignment.
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(
            generation,
            fake_candidate_alias(207, "owned.log"),
            Duration::ZERO,
        )
        .unwrap();
    let third = admission.finish_scan().unwrap();
    assert!(third.stats.complete);
    assert!(
        !third
            .events
            .iter()
            .any(|event| matches!(event, CandidateEvent::Updated(_)))
    );
}

/// Scenario: identity-event capacity for one reconciliation pass is fully
/// consumed by an already-tracked owner's own binding update (a rename),
/// while a validated move/create replacement claimant for that owner's old
/// path is simultaneously selected but cannot be admitted this same pass.
/// Guarantees: the claimant is pushed to the bounded pending queue rather
/// than dropped, this pass's `recognized_replacements` names only what it
/// actually emitted (the owner's rename, not the still-pending claimant),
/// and a later complete pass that re-observes the claimant emits it and
/// still reports it in `recognized_replacements`, so identity resolution
/// later bypasses `start_at` for it.
#[test]
fn claimant_deferred_by_owner_binding_update_is_recognized_when_later_emitted() {
    // `max_candidate_events == 1` forces the owner's own rename to consume
    // the pass's only event slot before the claimant can be admitted.
    let mut admission = AdmissionController::new(4, 4, 1, 16).unwrap();
    let owner_initial = fake_candidate_alias(300, "rotate.log");
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, owner_initial, Duration::ZERO)
        .unwrap();
    let first = admission.finish_scan().unwrap();
    assert_eq!(observed_candidates(&first).len(), 1);
    admission.apply_feedback(durable_feedback(&first)).unwrap();

    // The owner renames away from its old path, and a new object appears
    // at that same now-frozen path: a validated move/create replacement.
    let owner_moved = fake_candidate_alias(300, "moved-away.log");
    let claimant = fake_candidate_alias(301, "rotate.log");
    let claimant_locator = claimant.evidence.locator;
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(generation, owner_moved, Duration::ZERO)
        .unwrap();
    admission
        .observe(generation, claimant, Duration::ZERO)
        .unwrap();
    let second = admission.finish_scan().unwrap();

    // Only the owner's own rename emitted this pass; the claimant is
    // pending, not yet admitted, so it must not be named as a recognized
    // replacement until it actually emits.
    assert_eq!(observed_candidates(&second).len(), 0);
    let updated_locators: Vec<Locator> = second
        .events
        .iter()
        .filter_map(|event| match event {
            CandidateEvent::Updated(candidate) => Some(candidate.evidence.locator),
            _ => None,
        })
        .collect();
    assert_eq!(
        updated_locators,
        vec![Locator::PosixDevIno { dev: 1, ino: 300 }]
    );
    assert!(second.recognized_replacements.is_empty());
    assert_eq!(second.stats.pending_candidates, 1);
    admission.apply_feedback(durable_feedback(&second)).unwrap();

    // A later pass re-observes the still-pending claimant (unchanged) so
    // its pending entry is refreshed for this generation and can finally
    // be admitted.
    let generation = admission.begin_scan(SystemTime::now()).unwrap();
    admission
        .observe(
            generation,
            fake_candidate_alias(301, "rotate.log"),
            Duration::ZERO,
        )
        .unwrap();
    let third = admission.finish_scan().unwrap();

    let observed_this_pass = observed_candidates(&third);
    assert_eq!(observed_this_pass.len(), 1);
    assert_eq!(observed_this_pass[0].evidence.locator, claimant_locator);
    assert_eq!(
        third.recognized_replacements,
        HashSet::from([claimant_locator])
    );
}
