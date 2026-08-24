// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use tempfile::TempDir;

use super::admission::AdmissionController;
use super::scanner::{DiscoveryPlan, FilesystemScanner, validate_candidate_path_stability};
use super::source::spawn_discovery;
use super::*;
use crate::receivers::filelog_receiver::checkpoint::primitives::{
    FRAMING_PROFILE_VERSION, FileId, FramingResume,
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
    config.discovery.poll_interval = Duration::from_secs(60);
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

fn observed_candidates(batch: &ReconciliationBatch) -> Vec<&DiscoveredCandidate> {
    batch
        .events
        .iter()
        .filter_map(|event| match event {
            CandidateEvent::Observed(candidate) => Some(candidate),
            CandidateEvent::Updated(_) | CandidateEvent::Removed { .. } => None,
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
            advisory_path: format!("candidate-{number}.log").into_bytes(),
        },
        modified: None,
    }
}

fn durable_feedback(batch: &ReconciliationBatch) -> DiscoveryFeedback {
    DiscoveryFeedback {
        durable: event_locators(batch),
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
        vec![pattern(root, "excluded/**")],
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
    candidate.modified = Some(SystemTime::UNIX_EPOCH);

    let generation = admission.begin_scan(now).unwrap();
    admission
        .observe(generation, candidate.clone(), Duration::from_secs(10))
        .unwrap();
    let skipped = admission.finish_scan().unwrap();
    assert!(skipped.events.is_empty());

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
    assert_eq!(admission.tracked_locators().len(), 1);
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
/// Guarantees: candidate overflow disables fingerprint-only recovery but
/// does not suppress proven removal, and removals remain ordered after all
/// observed or updated transitions in the batch.
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
            durable: vec![locator],
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
    shutdown_requested.store(true, Ordering::Relaxed);

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
