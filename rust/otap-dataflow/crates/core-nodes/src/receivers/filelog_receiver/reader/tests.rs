// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;

use tempfile::tempdir;

use super::*;
use crate::receivers::filelog_receiver::checkpoint::AdvisoryPath;
use crate::receivers::filelog_receiver::config::{Config, RuntimeConfig};
use crate::receivers::filelog_receiver::identity::matcher::IdentityMatch;
use crate::receivers::filelog_receiver::identity::platform::open_candidate;

fn settings(max_readers: usize, max_open_files: usize, turn_bytes: usize) -> ReaderSettings {
    ReaderSettings {
        max_readers,
        max_open_files,
        max_read_bytes_per_turn: turn_bytes,
        eof_probe_interval: Duration::from_secs(1),
        fingerprint_bytes: 16,
        ignored_header_bytes: 0,
        follow_symlinks: false,
    }
}

fn settings_with_fingerprint(
    max_readers: usize,
    max_open_files: usize,
    turn_bytes: usize,
    fingerprint_bytes: u16,
) -> ReaderSettings {
    ReaderSettings {
        fingerprint_bytes,
        ..settings(max_readers, max_open_files, turn_bytes)
    }
}

/// Scenario: discovery reconciles once per day while admitted EOF readers
/// re-probe every 10 milliseconds.
/// Guarantees: `ReaderSettings` consumes only
/// `reader.eof_reprobe_interval`; discovery cadence cannot round EOF probes
/// up or trigger directory traversal.
#[test]
fn eof_reprobe_cadence_is_independent_from_reconciliation() {
    let mut config: Config = serde_json::from_value(serde_json::json!({
        "include": ["app.log"],
        "checkpoint": { "id": "reader-cadence" }
    }))
    .unwrap();
    config.discovery.reconcile_interval = Duration::from_secs(24 * 60 * 60);
    config.reader.eof_reprobe_interval = Duration::from_millis(10);
    let runtime = RuntimeConfig::from_config(config, "reader-cadence").unwrap();

    let settings = ReaderSettings::from_runtime(&runtime);
    assert_eq!(settings.eof_probe_interval, Duration::from_millis(10));
}

fn file_id(seed: u8) -> FileId {
    FileId::from_bytes([seed; 16])
}

fn candidate(path: &Path) -> DiscoveredCandidate {
    candidate_with_fingerprint(path, 16)
}

fn candidate_with_fingerprint(path: &Path, fingerprint_bytes: u16) -> DiscoveredCandidate {
    let resolved_path = std::fs::canonicalize(path).expect("candidate canonicalizes");
    let opened =
        open_candidate(&resolved_path, false, fingerprint_bytes, 0).expect("candidate opens");
    DiscoveredCandidate {
        matched_path: path.to_path_buf(),
        resolved_path,
        evidence: opened.evidence,
        modified: None,
    }
}

fn resolved(seed: u8, offset: u64) -> ResolvedIdentity {
    resolved_with_guard(seed, offset, CommittedFrontierGuard::empty())
}

fn resolved_with_guard(
    seed: u8,
    offset: u64,
    committed_frontier_guard: CommittedFrontierGuard,
) -> ResolvedIdentity {
    ResolvedIdentity {
        file_id: file_id(seed),
        file_epoch: 0,
        committed_offset: offset,
        framing_resume: FramingResume::Clean,
        lifecycle_state: LifecycleState::Active,
        matched_by: IdentityMatch::NewDiscovery,
        committed_frontier_guard,
        advisory_path: AdvisoryPath::unavailable(),
    }
}

/// Computes the real committed-frontier guard for a known literal prefix,
/// for tests that admit a genuinely new identity at an offset that does not
/// coincide with the candidate's own (EOF) window and so cannot rely on
/// [`ReaderTable::insert`] adopting that window directly; the reader must
/// then independently validate this guard once its descriptor opens.
fn guard_for_prefix(prefix: &[u8]) -> CommittedFrontierGuard {
    CommittedFrontierGuard::compute(prefix.len() as u64, prefix)
        .expect("test prefix must fit the committed-frontier guard window")
}

fn data(poll: ReaderPoll) -> ReadTurn {
    match poll {
        ReaderPoll::Data(turn) => turn,
        other => panic!("expected a data turn, got {other:?}"),
    }
}

fn eviction(poll: ReaderPoll) -> EvictionRequest {
    match poll {
        ReaderPoll::EvictionRequired(request) => request,
        other => panic!("expected an eviction request, got {other:?}"),
    }
}

fn environmental(
    poll: ReaderPoll,
) -> (
    FileId,
    EnvironmentalOperation,
    EnvironmentalErrorClass,
    Instant,
) {
    match poll {
        ReaderPoll::EnvironmentalBackoff {
            file_id,
            operation,
            error,
            next_probe,
        } => (file_id, operation, error, next_probe),
        other => panic!("expected environmental backoff, got {other:?}"),
    }
}

/// Scenario: two continuously readable files share one bounded scheduler.
/// Guarantees: each reader receives one source-byte turn before either gets
/// a second turn, every turn respects the byte cap, and per-file offsets are
/// monotonic.
#[test]
fn round_robin_turns_bound_bytes_and_preserve_file_order() {
    let directory = tempdir().unwrap();
    let first_path = directory.path().join("first.log");
    let second_path = directory.path().join("second.log");
    std::fs::write(&first_path, b"abcdef").unwrap();
    std::fs::write(&second_path, b"123456").unwrap();
    let mut table = ReaderTable::new(settings(2, 2, 2)).unwrap();
    table
        .insert(candidate(&first_path), resolved(1, 0))
        .unwrap();
    table
        .insert(candidate(&second_path), resolved(2, 0))
        .unwrap();
    let now = Instant::now();

    let first = data(table.poll(now).unwrap());
    assert_eq!(first.file_id(), file_id(1));
    assert_eq!(first.source_offset(), 0);
    assert_eq!(first.bytes(), b"ab");
    table
        .complete_turn(first, 2, TurnDisposition::Ready)
        .unwrap();

    let second = data(table.poll(now).unwrap());
    assert_eq!(second.file_id(), file_id(2));
    assert_eq!(second.source_offset(), 0);
    assert_eq!(second.bytes(), b"12");
    table
        .complete_turn(second, 2, TurnDisposition::Ready)
        .unwrap();

    let first_again = data(table.poll(now).unwrap());
    assert_eq!(first_again.file_id(), file_id(1));
    assert_eq!(first_again.source_offset(), 2);
    assert_eq!(first_again.bytes(), b"cd");
    table
        .complete_turn(first_again, 2, TurnDisposition::Paused)
        .unwrap();
    table.shutdown().unwrap();
}

/// Scenario: one resident reader's positioned read fails with a temporary
/// permission error while a second file is ready.
/// Guarantees: the failed file keeps its progress and descriptor, receives a
/// 250ms bounded retry, and the unrelated file continues immediately.
#[test]
fn per_file_read_error_backs_off_without_blocking_other_readers() {
    let directory = tempdir().unwrap();
    let first_path = directory.path().join("failed.log");
    let second_path = directory.path().join("healthy.log");
    std::fs::write(&first_path, b"fail").unwrap();
    std::fs::write(&second_path, b"okay").unwrap();
    let mut table = ReaderTable::new(settings(2, 2, 4)).unwrap();
    table
        .insert(candidate(&first_path), resolved(1, 0))
        .unwrap();
    table
        .insert(candidate(&second_path), resolved(2, 0))
        .unwrap();
    table.fail_next_read_for_test(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "temporary permission",
    ));
    let now = Instant::now();

    let (failed, operation, class, retry_at) = environmental(table.poll(now).unwrap());
    assert_eq!(failed, file_id(1));
    assert_eq!(operation, EnvironmentalOperation::Read);
    assert_eq!(class, EnvironmentalErrorClass::Permission);
    assert_eq!(retry_at.duration_since(now), Duration::from_millis(250));
    let healthy = data(table.poll(now).unwrap());
    assert_eq!(healthy.file_id(), file_id(2));
    assert_eq!(healthy.bytes(), b"okay");
    table
        .complete_turn(healthy, 4, TurnDisposition::Paused)
        .unwrap();

    let stats = table.stats();
    assert_eq!(stats.environmental_backoff_readers, 1);
    assert_eq!(stats.eof_readers, 0);
    assert_eq!(stats.removed_readers, 0);
    assert_eq!(table.frontier(file_id(1)).unwrap().committed_offset, 0);
    let recovered = data(table.poll(retry_at).unwrap());
    assert_eq!(recovered.file_id(), file_id(1));
    assert_eq!(recovered.bytes(), b"fail");
    table
        .complete_turn(recovered, 4, TurnDisposition::Paused)
        .unwrap();
    assert_eq!(table.stats().environmental_backoff_readers, 0);
    table.shutdown().unwrap();
}

/// Scenario: opening one descriptor-absent file fails with a temporary
/// permission error while another file is ready.
/// Guarantees: only the failed file receives per-file backoff; descriptor
/// pressure remains clear and the unrelated source opens and reads normally.
#[test]
fn per_file_open_error_does_not_pause_unrelated_reader() {
    let directory = tempdir().unwrap();
    let first_path = directory.path().join("failed-open.log");
    let second_path = directory.path().join("healthy-open.log");
    std::fs::write(&first_path, b"x").unwrap();
    std::fs::write(&second_path, b"y").unwrap();
    let mut table = ReaderTable::new(settings(2, 2, 1)).unwrap();
    table
        .insert(candidate(&first_path), resolved(1, 0))
        .unwrap();
    table
        .insert(candidate(&second_path), resolved(2, 0))
        .unwrap();
    table.fail_next_open_for_test(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "temporary permission",
    ));
    let now = Instant::now();

    let (failed, operation, class, retry_at) = environmental(table.poll(now).unwrap());
    assert_eq!(failed, file_id(1));
    assert_eq!(operation, EnvironmentalOperation::Open);
    assert_eq!(class, EnvironmentalErrorClass::Permission);
    assert_eq!(retry_at.duration_since(now), Duration::from_millis(250));
    assert_eq!(table.descriptor_pressure_deadline(now).unwrap(), None);

    let healthy = data(table.poll(now).unwrap());
    assert_eq!(healthy.file_id(), file_id(2));
    assert_eq!(healthy.bytes(), b"y");
    table
        .complete_turn(healthy, 1, TurnDisposition::Paused)
        .unwrap();
    assert_eq!(table.stats().environmental_backoff_readers, 1);
    assert_eq!(table.stats().removed_readers, 0);
    table.shutdown().unwrap();
}

/// Scenario: a present descriptor-absent reader's path temporarily returns
/// `NotFound` before discovery has produced removal evidence.
/// Guarantees: the reader preserves presence and progress under per-file
/// environmental backoff; source reading does not invent a removal event.
#[test]
fn reopen_not_found_waits_for_discovery_removal_evidence() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("temporarily-missing.log");
    std::fs::write(&path, b"x").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 1)).unwrap();
    table.insert(candidate(&path), resolved(1, 0)).unwrap();
    std::fs::remove_file(&path).unwrap();
    let now = Instant::now();

    let (failed_id, operation, class, retry_at) = environmental(table.poll(now).unwrap());
    assert_eq!(failed_id, file_id(1));
    assert_eq!(operation, EnvironmentalOperation::Open);
    assert_eq!(class, EnvironmentalErrorClass::Other);
    assert_eq!(retry_at.duration_since(now), Duration::from_millis(250));
    let frontier = table.frontier(failed_id).unwrap();
    assert!(frontier.present);
    assert_eq!(frontier.committed_offset, 0);
    assert_eq!(table.stats().removed_readers, 0);
    table.shutdown().unwrap();
}

/// Scenario: lifecycle pause arrives while a per-file environmental retry is
/// waiting on its deadline.
/// Guarantees: the wait is removed immediately without progress, while the
/// bounded failure count remains available if drain explicitly retries the
/// same source.
#[test]
fn lifecycle_pause_interrupts_environmental_retry_wait() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("paused-retry.log");
    std::fs::write(&path, b"x").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 1)).unwrap();
    table.insert(candidate(&path), resolved(1, 0)).unwrap();
    table.fail_next_read_for_test(io::Error::new(io::ErrorKind::WouldBlock, "retry"));
    let now = Instant::now();
    let (failed_id, _, _, _) = environmental(table.poll(now).unwrap());

    table.pause(failed_id).unwrap();
    assert_eq!(table.stats().environmental_backoff_readers, 0);
    assert_eq!(table.frontier(failed_id).unwrap().read_offset, 0);
    assert!(table.environmental_failures.contains_key(&failed_id));
    table.shutdown().unwrap();
}

#[cfg(unix)]
/// Scenario: the first descriptor open receives `EMFILE` while a second
/// descriptor-absent reader is ready.
/// Guarantees: one receiver-global 250ms deadline pauses both new opens,
/// neither reader is removed or quarantined, and a successful retry clears
/// descriptor pressure.
#[test]
fn emfile_pauses_new_opens_until_bounded_retry() {
    let directory = tempdir().unwrap();
    let first_path = directory.path().join("first.log");
    let second_path = directory.path().join("second.log");
    std::fs::write(&first_path, b"a").unwrap();
    std::fs::write(&second_path, b"b").unwrap();
    let mut table = ReaderTable::new(settings(2, 2, 1)).unwrap();
    table
        .insert(candidate(&first_path), resolved(1, 0))
        .unwrap();
    table
        .insert(candidate(&second_path), resolved(2, 0))
        .unwrap();
    table.fail_next_open_for_test(io::Error::from_raw_os_error(libc::EMFILE));
    let now = Instant::now();

    let (first, operation, class, retry_at) = environmental(table.poll(now).unwrap());
    assert_eq!(first, file_id(1));
    assert_eq!(operation, EnvironmentalOperation::Open);
    assert_eq!(class, EnvironmentalErrorClass::DescriptorPressure);
    assert_eq!(retry_at.duration_since(now), Duration::from_millis(250));
    let (second, _, second_class, second_retry) = environmental(table.poll(now).unwrap());
    assert_eq!(second, file_id(2));
    assert_eq!(second_class, EnvironmentalErrorClass::DescriptorPressure);
    assert_eq!(second_retry, retry_at);
    assert_eq!(table.stats().environmental_backoff_readers, 2);
    assert_eq!(
        table.descriptor_pressure_deadline(now).unwrap(),
        Some(retry_at)
    );

    let turn = data(table.poll(retry_at).unwrap());
    assert!([file_id(1), file_id(2)].contains(&turn.file_id()));
    assert_eq!(turn.bytes().len(), 1);
    table
        .complete_turn(turn, 1, TurnDisposition::Paused)
        .unwrap();
    assert_eq!(table.descriptor_pressure_deadline(retry_at).unwrap(), None);
    assert_eq!(table.stats().removed_readers, 0);
    table.shutdown().unwrap();
}

#[cfg(unix)]
/// Scenario: bounded drain replay selects a descriptor-absent reader whose
/// first open receives `EMFILE`.
/// Guarantees: drain polling returns the same interruptible environmental
/// deadline until it expires instead of reporting an idle scheduler or
/// retrying the open early.
#[test]
fn drain_poll_preserves_active_descriptor_pressure_wait() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("drain-pressure.log");
    std::fs::write(&path, b"x").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 1)).unwrap();
    table.insert(candidate(&path), resolved(1, 0)).unwrap();
    table.fail_next_open_for_test(io::Error::from_raw_os_error(libc::EMFILE));
    let now = Instant::now();

    let (_, operation, class, retry_at) =
        environmental(table.poll_until(now, file_id(1), 1).unwrap());
    assert_eq!(operation, EnvironmentalOperation::Open);
    assert_eq!(class, EnvironmentalErrorClass::DescriptorPressure);

    let before_retry = now + Duration::from_millis(10);
    let (_, repeated_operation, repeated_class, repeated_retry_at) =
        environmental(table.poll_until(before_retry, file_id(1), 1).unwrap());
    assert_eq!(repeated_operation, operation);
    assert_eq!(repeated_class, class);
    assert_eq!(repeated_retry_at, retry_at);

    let turn = data(table.poll_until(retry_at, file_id(1), 1).unwrap());
    assert_eq!(turn.file_id(), file_id(1));
    assert_eq!(turn.bytes(), b"x");
    table
        .complete_turn(turn, 1, TurnDisposition::Paused)
        .unwrap();
    table.shutdown().unwrap();
}

/// Scenario: sealing a receiver-wide batch rewinds three ready readers in an
/// order different from their established round-robin queue.
/// Guarantees: the Ack resume preserves the exact pre-seal queue order rather
/// than rebuilding it from unordered reader-table iteration.
#[test]
fn batch_pause_preserves_exact_ready_queue_order() {
    let directory = tempdir().unwrap();
    let paths: Vec<_> = (1..=3)
        .map(|seed| {
            let path = directory.path().join(format!("{seed}.log"));
            std::fs::write(&path, b"ab").unwrap();
            path
        })
        .collect();
    let mut table = ReaderTable::new(settings(3, 3, 1)).unwrap();
    for seed in 1..=3 {
        table
            .insert(candidate(&paths[usize::from(seed - 1)]), resolved(seed, 0))
            .unwrap();
    }
    let now = Instant::now();

    for expected in [1, 2] {
        let turn = data(table.poll(now).unwrap());
        assert_eq!(turn.file_id(), file_id(expected));
        table
            .complete_turn(turn, 1, TurnDisposition::Ready)
            .unwrap();
    }
    assert_eq!(
        table.ready.iter().copied().collect::<Vec<_>>(),
        [file_id(3), file_id(1), file_id(2)]
    );

    table.prepare_batch_pause_order().unwrap();
    for seed in [2, 1, 3] {
        table
            .rewind_provisional_frontier(file_id(seed), 0, 0)
            .unwrap();
    }
    assert_eq!(
        table.ready.iter().copied().collect::<Vec<_>>(),
        [file_id(3), file_id(1), file_id(2)]
    );
    table.finish_batch_commit(true).unwrap();

    for expected in [3, 1, 2] {
        let turn = data(table.poll(now).unwrap());
        assert_eq!(turn.file_id(), file_id(expected));
        table
            .complete_turn(turn, 0, TurnDisposition::Ready)
            .unwrap();
    }
    table.shutdown().unwrap();
}

/// Scenario: framing consumes only a prefix of a bounded source turn before
/// reaching an external batch boundary.
/// Guarantees: the scheduler advances by exactly that prefix and rereads the
/// returned turn's unconsumed suffix without loss.
#[test]
fn partial_turn_consumption_rereads_unconsumed_suffix() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("partial.log");
    std::fs::write(&path, b"abcd").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 3)).unwrap();
    table.insert(candidate(&path), resolved(3, 0)).unwrap();
    let now = Instant::now();

    let first = data(table.poll(now).unwrap());
    assert_eq!(first.bytes(), b"abc");
    table
        .complete_turn(first, 1, TurnDisposition::Ready)
        .unwrap();

    let replay = data(table.poll(now).unwrap());
    assert_eq!(replay.source_offset(), 1);
    assert_eq!(replay.bytes(), b"bcd");
    table
        .complete_turn(replay, 3, TurnDisposition::Paused)
        .unwrap();
    table.shutdown().unwrap();
}

/// Scenario: a growing regular file reaches temporary EOF and receives a
/// later append without a discovery metadata transition.
/// Guarantees: EOF does not spin or finalize the reader, and the deadline
/// re-probe resumes at the exact prior source-byte frontier.
#[test]
fn temporary_eof_reactivates_on_deadline_after_append() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("growing.log");
    std::fs::write(&path, b"a").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 4)).unwrap();
    table.insert(candidate(&path), resolved(4, 0)).unwrap();
    let now = Instant::now();

    let first = data(table.poll(now).unwrap());
    assert_eq!(first.bytes(), b"a");
    table
        .complete_turn(first, 1, TurnDisposition::Ready)
        .unwrap();
    let next_probe = match table.poll(now).unwrap() {
        ReaderPoll::EndOfFile {
            source_offset,
            next_probe,
            ..
        } => {
            assert_eq!(source_offset, 1);
            next_probe
        }
        other => panic!("expected EOF, got {other:?}"),
    };
    assert!(matches!(
        table.poll(now).unwrap(),
        ReaderPoll::Idle {
            next_probe: Some(_)
        }
    ));

    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"b")
        .unwrap();
    assert!(matches!(
        table.poll(now).unwrap(),
        ReaderPoll::Idle {
            next_probe: Some(_)
        }
    ));

    let appended = data(table.poll(next_probe).unwrap());
    assert_eq!(appended.source_offset(), 1);
    assert_eq!(appended.bytes(), b"b");
    table
        .complete_turn(appended, 1, TurnDisposition::Paused)
        .unwrap();
    let stats = table.stats();
    assert_eq!(stats.eof_reprobes, 1);
    assert_eq!(stats.read_turns, 3);
    table.shutdown().unwrap();
}

/// Scenario: an empty start-at-end reader opens and immediately reaches EOF.
/// Guarantees: the current population reports one resident descriptor and
/// one EOF reader without requiring another successful data turn.
#[test]
fn empty_eof_open_updates_current_populations_immediately() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("empty.log");
    std::fs::write(&path, b"").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 4)).unwrap();
    table.insert(candidate(&path), resolved(54, 0)).unwrap();

    assert!(matches!(
        table.poll(Instant::now()).unwrap(),
        ReaderPoll::EndOfFile { .. }
    ));
    let stats = table.stats();
    assert_eq!(stats.open_files, 1);
    assert_eq!(stats.eof_readers, 1);
    assert_eq!(stats.descriptor_blocked_readers, 0);
    assert_eq!(stats.removed_readers, 0);
    table.shutdown().unwrap();
}

/// Scenario: three ready readers compete for two descriptor slots after the
/// first two have each been served.
/// Guarantees: opening the third selects the least-recently-served descriptor
/// and requires explicit discard confirmation before closing it.
#[test]
fn descriptor_rotation_selects_lrs_and_requires_confirmation() {
    let directory = tempdir().unwrap();
    let paths: Vec<_> = (1..=3)
        .map(|seed| {
            let path = directory.path().join(format!("{seed}.log"));
            std::fs::write(&path, [b'0' + seed]).unwrap();
            path
        })
        .collect();
    let mut table = ReaderTable::new(settings(3, 2, 1)).unwrap();
    for seed in 1..=3 {
        table
            .insert(candidate(&paths[usize::from(seed - 1)]), resolved(seed, 0))
            .unwrap();
    }
    let now = Instant::now();

    for expected in [1, 2] {
        let turn = data(table.poll(now).unwrap());
        assert_eq!(turn.file_id(), file_id(expected));
        table
            .complete_turn(turn, 1, TurnDisposition::Ready)
            .unwrap();
    }

    let request = eviction(table.poll(now).unwrap());
    assert_eq!(request.target_file_id, file_id(3));
    assert_eq!(request.victim_file_id, file_id(1));
    assert_eq!(request.committed_offset, 0);
    assert_eq!(request.read_offset, 1);
    assert!(matches!(
        table.poll(now).unwrap(),
        ReaderPoll::EvictionRequired(repeated) if repeated == request
    ));
    table.confirm_eviction(request).unwrap();
    assert_eq!(table.stats().open_files, 1);
    assert_eq!(table.stats().source_bytes_rewound, 1);
    assert_eq!(table.stats().descriptor_evictions, 1);

    let third = data(table.poll(now).unwrap());
    assert_eq!(third.file_id(), file_id(3));
    table
        .complete_turn(third, 1, TurnDisposition::Paused)
        .unwrap();
    assert_eq!(table.stats().open_files, 2);
    table.shutdown().unwrap();
}

/// Scenario: a logical reader's descriptor rotates out while another
/// receiver scope attempts to acquire the same native locator.
/// Guarantees: temporary descriptor closure retains the runtime lease, while
/// explicit removed-reader finalization releases it.
#[test]
fn runtime_lease_survives_eviction_until_finalization() {
    let directory = tempdir().unwrap();
    let first_path = directory.path().join("leased.log");
    let second_path = directory.path().join("other.log");
    std::fs::write(&first_path, b"a").unwrap();
    std::fs::write(&second_path, b"b").unwrap();
    let first_candidate = candidate(&first_path);
    let locator = first_candidate.evidence.locator;
    let mut table = ReaderTable::new(settings(2, 1, 1)).unwrap();
    table.insert(first_candidate, resolved(5, 0)).unwrap();
    table
        .insert(candidate(&second_path), resolved(6, 0))
        .unwrap();
    let now = Instant::now();

    let first = data(table.poll(now).unwrap());
    table
        .complete_turn(first, 1, TurnDisposition::Ready)
        .unwrap();
    let request = eviction(table.poll(now).unwrap());
    table.confirm_eviction(request).unwrap();

    let mut other_scope = register_receiver_scope(1).unwrap();
    assert!(matches!(
        other_scope.try_acquire(locator),
        Err(LeaseError::Contended { .. })
    ));
    assert_eq!(
        table.mark_removed(locator).unwrap(),
        RemovalDisposition::DescriptorAbsent
    );
    assert_eq!(table.release_finalized(file_id(5)).unwrap(), locator);
    let acquired = other_scope.try_acquire(locator).unwrap();
    acquired.release().unwrap();
    other_scope.close().unwrap();
    table.shutdown().unwrap();
}

/// Scenario: every descriptor slot belongs to a removed reader whose open
/// handle is required for best-effort late-write capture.
/// Guarantees: the scheduler pins that handle, reports bounded descriptor
/// pressure, and does not silently release its runtime lease or finalize it.
#[test]
fn removed_open_handle_is_not_an_lrs_victim() {
    let directory = tempdir().unwrap();
    let first_path = directory.path().join("rotated.log");
    let second_path = directory.path().join("replacement.log");
    std::fs::write(&first_path, b"a").unwrap();
    std::fs::write(&second_path, b"b").unwrap();
    let first_candidate = candidate(&first_path);
    let locator = first_candidate.evidence.locator;
    let mut table = ReaderTable::new(settings(2, 1, 1)).unwrap();
    table.insert(first_candidate, resolved(7, 0)).unwrap();
    table
        .insert(candidate(&second_path), resolved(8, 0))
        .unwrap();
    let now = Instant::now();

    let first = data(table.poll(now).unwrap());
    table
        .complete_turn(first, 1, TurnDisposition::Ready)
        .unwrap();
    let pinned_since = now - Duration::from_millis(25);
    assert_eq!(
        table.mark_removed_at(locator, pinned_since).unwrap(),
        RemovalDisposition::HandleRetained
    );
    let pinned = table.stats_at(now);
    assert_eq!(pinned.pinned_rotated_handles, 1);
    assert!(pinned.pinned_rotated_oldest_age_ns >= 25_000_000);
    assert!(matches!(
        table.poll(now).unwrap(),
        ReaderPoll::DescriptorCapacityBlocked { file_id: blocked } if blocked == file_id(8)
    ));
    assert_eq!(table.stats().open_files, 1);
    assert_eq!(table.stats().removed_readers, 1);

    table.pause(file_id(7)).unwrap();
    let _released = table.release_finalized(file_id(7)).unwrap();
    let released = table.stats_at(now);
    assert_eq!(released.pinned_rotated_handles, 0);
    assert_eq!(released.pinned_rotated_oldest_age_ns, 0);
    let replacement = data(table.poll(now).unwrap());
    assert_eq!(replacement.file_id(), file_id(8));
    table
        .complete_turn(replacement, 1, TurnDisposition::Paused)
        .unwrap();
    table.shutdown().unwrap();
}

/// Scenario: a reader opens successfully, is evicted for another reader, and
/// then its next descriptor reopen fails.
/// Guarantees: only the failed reopen increments the bounded monotonic reopen
/// failure counter; the initial open is not misclassified.
#[test]
fn descriptor_reopen_failure_is_counted_after_prior_open() {
    let directory = tempdir().unwrap();
    let first_path = directory.path().join("first.log");
    let second_path = directory.path().join("second.log");
    std::fs::write(&first_path, b"a").unwrap();
    std::fs::write(&second_path, b"b").unwrap();
    let mut table = ReaderTable::new(settings(2, 1, 1)).unwrap();
    table
        .insert(candidate(&first_path), resolved(58, 0))
        .unwrap();
    table
        .insert(candidate(&second_path), resolved(59, 0))
        .unwrap();
    let now = Instant::now();

    let first = data(table.poll(now).unwrap());
    table
        .complete_turn(first, 1, TurnDisposition::Ready)
        .unwrap();
    let request = eviction(table.poll(now).unwrap());
    table.confirm_eviction(request).unwrap();
    let second = data(table.poll(now).unwrap());
    table
        .complete_turn(second, 1, TurnDisposition::Paused)
        .unwrap();
    let reopen_request = eviction(table.poll(now).unwrap());
    assert_eq!(reopen_request.target_file_id, file_id(58));
    table.confirm_eviction(reopen_request).unwrap();
    table.fail_next_open_for_test(io::Error::from(io::ErrorKind::PermissionDenied));

    let result = table.poll(now);
    assert!(
        matches!(
        result,
        Ok(ReaderPoll::EnvironmentalBackoff {
            file_id: failed,
            operation: EnvironmentalOperation::Open,
            error: EnvironmentalErrorClass::Permission,
            ..
        }) if failed == file_id(58)
        ),
        "{result:?}"
    );
    assert_eq!(table.stats().opens, 2);
    assert_eq!(table.stats().reopens, 0);
    assert_eq!(table.stats().descriptor_reopen_failures, 1);
    table.shutdown().unwrap();
}

/// Scenario: a closed logical reader's path is replaced by a different
/// native file before the reader is scheduled again.
/// Guarantees: reopen identity validation reports the old reader as removed
/// before any replacement byte is read under its durable identity.
#[test]
fn reopen_reports_replacement_locator_as_descriptor_unavailable() {
    let directory = tempdir().unwrap();
    let first_path = directory.path().join("replace.log");
    let second_path = directory.path().join("other.log");
    std::fs::write(&first_path, b"a").unwrap();
    std::fs::write(&second_path, b"b").unwrap();
    let mut table = ReaderTable::new(settings(2, 1, 1)).unwrap();
    table
        .insert(candidate(&first_path), resolved(9, 0))
        .unwrap();
    table
        .insert(candidate(&second_path), resolved(10, 0))
        .unwrap();
    let now = Instant::now();

    let first = data(table.poll(now).unwrap());
    table
        .complete_turn(first, 1, TurnDisposition::Ready)
        .unwrap();
    let request = eviction(table.poll(now).unwrap());
    table.confirm_eviction(request).unwrap();
    std::fs::remove_file(&first_path).unwrap();
    std::fs::write(&first_path, b"x").unwrap();

    let second = data(table.poll(now).unwrap());
    table
        .complete_turn(second, 1, TurnDisposition::Ready)
        .unwrap();
    let request = eviction(table.poll(now).unwrap());
    table.confirm_eviction(request).unwrap();
    assert!(matches!(
        table.poll(now).unwrap(),
        ReaderPoll::RemovedWithoutDescriptor {
            file_id: unavailable
        } if unavailable == file_id(9)
    ));
    assert!(!table.frontier(file_id(9)).unwrap().present);
    table.shutdown().unwrap();
}

/// Scenario: a temporarily closed short file grows while retaining its
/// native locator and original fingerprint prefix.
/// Guarantees: reopen accepts evidence growth and resumes from the durable
/// source-byte frontier rather than treating growth as replacement.
#[test]
fn reopen_accepts_same_locator_fingerprint_growth() {
    let directory = tempdir().unwrap();
    let first_path = directory.path().join("grow.log");
    let second_path = directory.path().join("other.log");
    std::fs::write(&first_path, b"a").unwrap();
    std::fs::write(&second_path, b"z").unwrap();
    let mut table = ReaderTable::new(settings(2, 1, 1)).unwrap();
    table
        .insert(candidate(&first_path), resolved(11, 1))
        .unwrap();
    table
        .insert(candidate(&second_path), resolved(12, 0))
        .unwrap();
    let now = Instant::now();

    let eof = table.poll(now).unwrap();
    assert!(matches!(
        eof,
        ReaderPoll::EndOfFile {
            file_id: eof_file_id,
            source_offset: 1,
            ..
        } if eof_file_id == file_id(11)
    ));
    table.make_ready(file_id(12)).unwrap();
    let request = eviction(table.poll(now).unwrap());
    table.confirm_eviction(request).unwrap();
    OpenOptions::new()
        .append(true)
        .open(&first_path)
        .unwrap()
        .write_all(b"b")
        .unwrap();

    let other = data(table.poll(now).unwrap());
    table
        .complete_turn(other, 1, TurnDisposition::Paused)
        .unwrap();
    table.make_ready(file_id(11)).unwrap();
    let request = eviction(table.poll(now).unwrap());
    table.confirm_eviction(request).unwrap();
    let grown = data(table.poll(now).unwrap());
    assert_eq!(grown.file_id(), file_id(11));
    assert_eq!(grown.source_offset(), 1);
    assert_eq!(grown.bytes(), b"b");
    table
        .complete_turn(grown, 1, TurnDisposition::Paused)
        .unwrap();
    table.shutdown().unwrap();
}

/// Scenario: a reader has consumed beyond its durable Ack frontier before
/// descriptor rotation.
/// Guarantees: only monotonic progress within the consumed range is
/// accepted, and confirmed eviction rewinds exactly to the latest committed
/// offset while preserving its framing-resume state.
#[test]
fn committed_frontier_bounds_progress_and_eviction_rewind() {
    let directory = tempdir().unwrap();
    let first_path = directory.path().join("progress.log");
    let second_path = directory.path().join("other.log");
    std::fs::write(&first_path, b"abcd").unwrap();
    std::fs::write(&second_path, b"z").unwrap();
    let mut table = ReaderTable::new(settings(2, 1, 4)).unwrap();
    table
        .insert(candidate(&first_path), resolved(13, 0))
        .unwrap();
    table
        .insert(candidate(&second_path), resolved(14, 0))
        .unwrap();
    let now = Instant::now();

    let first = data(table.poll(now).unwrap());
    table
        .complete_turn(first, 4, TurnDisposition::Ready)
        .unwrap();
    table
        .observe_committed_progress(file_id(13), 0, 2, FramingResume::Clean)
        .unwrap();
    assert!(matches!(
        table.observe_committed_progress(file_id(13), 0, 5, FramingResume::Clean),
        Err(ReaderError::InvalidProgress { .. })
    ));

    let request = eviction(table.poll(now).unwrap());
    assert_eq!(request.committed_offset, 2);
    assert_eq!(request.read_offset, 4);
    table.confirm_eviction(request).unwrap();
    assert_eq!(table.stats().source_bytes_rewound, 2);
    table.shutdown().unwrap();
}

/// Scenario: a stale epoch and a backward source offset are presented as
/// supposedly Ack-gated progress.
/// Guarantees: neither observation mutates the reader's durable frontier.
#[test]
fn stale_or_backward_progress_fails_closed() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("stale.log");
    std::fs::write(&path, b"abc").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 3)).unwrap();
    table
        .insert(
            candidate(&path),
            resolved_with_guard(15, 1, guard_for_prefix(b"a")),
        )
        .unwrap();
    let now = Instant::now();

    let turn = data(table.poll(now).unwrap());
    table
        .complete_turn(turn, 2, TurnDisposition::Paused)
        .unwrap();
    table
        .observe_committed_progress(file_id(15), 0, 1, FramingResume::Clean)
        .unwrap();
    assert!(matches!(
        table.observe_committed_progress(file_id(15), 1, 2, FramingResume::Clean),
        Err(ReaderError::InvalidProgress { .. })
    ));
    assert!(matches!(
        table.observe_committed_progress(file_id(15), 0, 0, FramingResume::Clean),
        Err(ReaderError::InvalidProgress { .. })
    ));
    table.shutdown().unwrap();
}

/// Scenario: a two-file checkpoint transaction contains one valid reader
/// frontier and one frontier beyond the bytes consumed in memory.
/// Guarantees: preflighting every reader detects the invalid member before
/// either durable frontier is changed.
#[test]
fn multi_file_progress_preflight_prevents_partial_application() {
    let directory = tempdir().unwrap();
    let first_path = directory.path().join("first.log");
    let second_path = directory.path().join("second.log");
    std::fs::write(&first_path, b"abc").unwrap();
    std::fs::write(&second_path, b"xyz").unwrap();
    let mut table = ReaderTable::new(settings(2, 2, 3)).unwrap();
    table
        .insert(candidate(&first_path), resolved(57, 0))
        .unwrap();
    table
        .insert(candidate(&second_path), resolved(58, 0))
        .unwrap();
    let now = Instant::now();

    let first = data(table.poll(now).unwrap());
    let first_id = first.file_id();
    table
        .complete_turn(first, 3, TurnDisposition::Paused)
        .unwrap();
    let second = data(table.poll(now).unwrap());
    let second_id = second.file_id();
    table
        .complete_turn(second, 3, TurnDisposition::Paused)
        .unwrap();

    table.preflight_committed_progress(first_id, 0, 2).unwrap();
    assert!(matches!(
        table.preflight_committed_progress(second_id, 0, 4),
        Err(ReaderError::InvalidProgress { .. })
    ));
    assert_eq!(table.frontier(first_id).unwrap().committed_offset, 0);
    assert_eq!(table.frontier(second_id).unwrap().committed_offset, 0);
    table.shutdown().unwrap();
}

/// Scenario: the scheduler's monotonic service sequence has exhausted its
/// representation before another read turn.
/// Guarantees: recency ordering fails closed instead of wrapping and
/// selecting an incorrect LRS descriptor.
#[test]
fn service_sequence_overflow_fails_closed() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("overflow.log");
    std::fs::write(&path, b"a").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 1)).unwrap();
    table.insert(candidate(&path), resolved(16, 0)).unwrap();
    table.set_service_sequence_for_test(u64::MAX);

    assert!(matches!(
        table.poll(Instant::now()),
        Err(ReaderError::CounterOverflow {
            counter: "reader service sequence"
        })
    ));
    table.shutdown().unwrap();
}

/// Scenario: the reader table is shut down while a source-byte turn remains
/// outside the table and uncommitted.
/// Guarantees: shutdown advances no progress and still releases the logical
/// reader's runtime lease and receiver scope.
#[test]
fn shutdown_releases_lease_with_uncommitted_turn() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("shutdown.log");
    std::fs::write(&path, b"a").unwrap();
    let observed = candidate(&path);
    let locator = observed.evidence.locator;
    let mut table = ReaderTable::new(settings(1, 1, 1)).unwrap();
    table.insert(observed, resolved(17, 0)).unwrap();
    let turn = data(table.poll(Instant::now()).unwrap());

    table.shutdown().unwrap();
    let mut other_scope = register_receiver_scope(1).unwrap();
    let acquired = other_scope.try_acquire(locator).unwrap();
    acquired.release().unwrap();
    other_scope.close().unwrap();
    drop(turn);
}

/// Scenario: independently constructed settings contain zero populations,
/// an overlarge descriptor population, or a zero EOF cadence.
/// Guarantees: the table rejects each invalid bound before registering a
/// lease scope or admitting a reader.
#[test]
fn invalid_reader_settings_are_rejected_before_use() {
    let mut invalid = settings(1, 1, 1);
    invalid.max_readers = 0;
    assert!(matches!(
        ReaderTable::new(invalid),
        Err(ReaderError::InvalidSettings { .. })
    ));

    let mut invalid = settings(1, 1, 1);
    invalid.max_open_files = 2;
    assert!(matches!(
        ReaderTable::new(invalid),
        Err(ReaderError::InvalidSettings { .. })
    ));

    let mut invalid = settings(1, 1, 1);
    invalid.eof_probe_interval = Duration::ZERO;
    assert!(matches!(
        ReaderTable::new(invalid),
        Err(ReaderError::InvalidSettings { .. })
    ));
}

/// Scenario: a closed present reader is blocked behind a removed reader
/// whose descriptor is pinned for late-write capture.
/// Guarantees: the blocked reader leaves the ready queue, the removed reader
/// can observe EOF and a later append, and releasing it automatically wakes
/// the descriptor waiter.
#[test]
fn descriptor_waiter_does_not_starve_removed_resident_reader() {
    let directory = tempdir().unwrap();
    let rotated_path = directory.path().join("rotated.log");
    let waiting_path = directory.path().join("waiting.log");
    std::fs::write(&rotated_path, b"a").unwrap();
    std::fs::write(&waiting_path, b"b").unwrap();
    let rotated = candidate(&rotated_path);
    let locator = rotated.evidence.locator;
    let mut table = ReaderTable::new(settings(2, 1, 1)).unwrap();
    table.insert(rotated, resolved(18, 0)).unwrap();
    table
        .insert(candidate(&waiting_path), resolved(19, 0))
        .unwrap();
    let now = Instant::now();

    let initial = data(table.poll(now).unwrap());
    table
        .complete_turn(initial, 1, TurnDisposition::Ready)
        .unwrap();
    assert_eq!(
        table.mark_removed(locator).unwrap(),
        RemovalDisposition::HandleRetained
    );
    assert!(matches!(
        table.poll(now).unwrap(),
        ReaderPoll::DescriptorCapacityBlocked { file_id: blocked }
            if blocked == file_id(19)
    ));
    let next_probe = match table.poll(now).unwrap() {
        ReaderPoll::EndOfFile {
            file_id: eof_file,
            next_probe,
            ..
        } => {
            assert_eq!(eof_file, file_id(18));
            next_probe
        }
        other => panic!("expected removed reader EOF, got {other:?}"),
    };

    OpenOptions::new()
        .append(true)
        .open(&rotated_path)
        .unwrap()
        .write_all(b"c")
        .unwrap();
    let late = data(table.poll(next_probe).unwrap());
    assert_eq!(late.file_id(), file_id(18));
    assert_eq!(late.source_offset(), 1);
    assert_eq!(late.bytes(), b"c");
    table
        .complete_turn(late, 1, TurnDisposition::Paused)
        .unwrap();
    let _released = table.release_finalized(file_id(18)).unwrap();

    let waiting = data(table.poll(next_probe).unwrap());
    assert_eq!(waiting.file_id(), file_id(19));
    table
        .complete_turn(waiting, 1, TurnDisposition::Paused)
        .unwrap();
    table.shutdown().unwrap();
}

/// Scenario: discovery removes the selected LRS victim after an eviction
/// request is issued but before the caller confirms discard.
/// Guarantees: removal cancels the stale request, confirmation cannot close
/// the now-pinned handle, and the removed reader remains serviceable.
#[test]
fn removal_cancels_pending_victim_eviction() {
    let directory = tempdir().unwrap();
    let victim_path = directory.path().join("victim.log");
    let target_path = directory.path().join("target.log");
    std::fs::write(&victim_path, b"a").unwrap();
    std::fs::write(&target_path, b"b").unwrap();
    let victim = candidate(&victim_path);
    let locator = victim.evidence.locator;
    let mut table = ReaderTable::new(settings(2, 1, 1)).unwrap();
    table.insert(victim, resolved(20, 0)).unwrap();
    table
        .insert(candidate(&target_path), resolved(21, 0))
        .unwrap();
    let now = Instant::now();

    let first = data(table.poll(now).unwrap());
    table
        .complete_turn(first, 1, TurnDisposition::Ready)
        .unwrap();
    let request = eviction(table.poll(now).unwrap());
    assert_eq!(request.victim_file_id, file_id(20));
    assert_eq!(
        table.mark_removed(locator).unwrap(),
        RemovalDisposition::HandleRetained
    );
    assert!(matches!(
        table.confirm_eviction(request),
        Err(ReaderError::InvalidTurn { .. })
    ));
    assert!(matches!(
        table.poll(now).unwrap(),
        ReaderPoll::DescriptorCapacityBlocked { file_id: blocked }
            if blocked == file_id(21)
    ));
    assert!(matches!(
        table.poll(now).unwrap(),
        ReaderPoll::EndOfFile {
            file_id: eof_file,
            ..
        } if eof_file == file_id(20)
    ));
    let _released = table.release_finalized(file_id(20)).unwrap();
    table.shutdown().unwrap();
}

/// Scenario: discovery removes the closed eviction target while a discard
/// request for another reader is outstanding.
/// Guarantees: removal cancels and pauses the target, stale confirmation is
/// rejected, and the unrelated resident descriptor remains open.
#[test]
fn removal_cancels_pending_target_eviction() {
    let directory = tempdir().unwrap();
    let resident_path = directory.path().join("resident.log");
    let target_path = directory.path().join("target.log");
    std::fs::write(&resident_path, b"a").unwrap();
    std::fs::write(&target_path, b"b").unwrap();
    let target = candidate(&target_path);
    let target_locator = target.evidence.locator;
    let mut table = ReaderTable::new(settings(2, 1, 1)).unwrap();
    table
        .insert(candidate(&resident_path), resolved(22, 0))
        .unwrap();
    table.insert(target, resolved(23, 0)).unwrap();
    let now = Instant::now();

    let resident = data(table.poll(now).unwrap());
    table
        .complete_turn(resident, 1, TurnDisposition::Ready)
        .unwrap();
    let request = eviction(table.poll(now).unwrap());
    assert_eq!(request.target_file_id, file_id(23));
    assert_eq!(
        table.mark_removed(target_locator).unwrap(),
        RemovalDisposition::DescriptorAbsent
    );
    assert!(matches!(
        table.confirm_eviction(request),
        Err(ReaderError::InvalidTurn { .. })
    ));
    let _released = table.release_finalized(file_id(23)).unwrap();
    assert!(matches!(
        table.poll(now).unwrap(),
        ReaderPoll::EndOfFile {
            file_id: eof_file,
            ..
        } if eof_file == file_id(22)
    ));
    table.shutdown().unwrap();
}

/// Scenario: an open file shrinks below either uncommitted read progress or
/// the Ack-gated committed frontier while retaining its first fingerprint
/// window.
/// Guarantees: a zero-byte read reports explicit truncation evidence instead
/// of ordinary EOF for both observable regressions.
#[test]
fn eof_reports_size_regression_even_when_fingerprint_prefix_survives() {
    for (seed, committed_offset) in [(24, 18), (25, 32)] {
        let directory = tempdir().unwrap();
        let path = directory.path().join("truncate.log");
        std::fs::write(&path, b"0123456789abcdefghijklmnopqrstuv").unwrap();
        let mut table = ReaderTable::new(settings(1, 1, 64)).unwrap();
        table.insert(candidate(&path), resolved(seed, 0)).unwrap();
        let now = Instant::now();

        let turn = data(table.poll(now).unwrap());
        assert_eq!(turn.bytes().len(), 32);
        table
            .complete_turn(turn, 32, TurnDisposition::Ready)
            .unwrap();
        table
            .observe_committed_progress(file_id(seed), 0, committed_offset, FramingResume::Clean)
            .unwrap();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(20)
            .unwrap();

        assert!(matches!(
            table.poll(now).unwrap(),
            ReaderPoll::Truncated {
                file_id: truncated,
                committed_offset: committed,
                read_offset: 32,
                observed_size: 20,
                ..
            } if truncated == file_id(seed) && committed == committed_offset
        ));
        table.shutdown().unwrap();
    }
}

/// Scenario: a live file is rewritten to the same size after its original
/// prefix was consumed, so size alone cannot reveal the replacement.
/// Guarantees: EOF prefix revalidation reports bounded truncation evidence
/// and never treats the rewritten stream as an ordinary append-only EOF.
#[test]
fn eof_reports_fingerprint_mismatch_without_size_regression() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("rewrite.log");
    std::fs::write(&path, b"0123456789abcdef\n").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 64)).unwrap();
    table.insert(candidate(&path), resolved(26, 0)).unwrap();
    let now = Instant::now();

    let turn = data(table.poll(now).unwrap());
    let consumed = turn.bytes().len();
    table
        .complete_turn(turn, consumed, TurnDisposition::Ready)
        .unwrap();
    std::fs::write(&path, b"fedcba9876543210\n").unwrap();

    assert!(matches!(
        table.poll(now).unwrap(),
        ReaderPoll::Truncated {
            file_id: rewritten,
            observed_size: 17,
            observed_fingerprint,
            ..
        } if rewritten == file_id(26) && observed_fingerprint == b"fedcba9876543210"
    ));
    table.shutdown().unwrap();
}

/// Scenario: a file changes between the two bounded EOF fingerprint
/// observations.
/// Guarantees: unstable evidence is classified for retry rather than as a
/// receiver-terminal identity failure.
#[test]
fn unstable_eof_fingerprint_evidence_is_retryable() {
    let result = classify_cancellable_fingerprint_observation(Err(
        IdentityError::CandidateChangedDuringIdentity {
            path: Path::new("changing.log").to_path_buf(),
        },
    ))
    .unwrap()
    .unwrap();
    assert_eq!(result, FingerprintObservation::Retry);
}

/// Scenario: a queued candidate's retained handle observes a same-size
/// rewrite between its two bounded fingerprint samples.
/// Guarantees: refresh returns a retry outcome, leaves the queued evidence
/// unchanged, and does not promote unstable identity evidence to an error.
#[test]
fn queued_candidate_refresh_retries_unstable_evidence() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("unstable-queued.log");
    std::fs::write(&path, b"a").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 1)).unwrap();
    table.insert(candidate(&path), resolved(57, 0)).unwrap();
    let turn = data(table.poll(Instant::now()).unwrap());
    table
        .complete_turn(turn, 1, TurnDisposition::Paused)
        .unwrap();
    let mut queued = candidate(&path);
    let original = queued.evidence.clone();
    let gate = table.gate_next_evidence_refresh_after_first_sample_for_test();
    let writer_gate = gate.clone();
    let writer_path = path.clone();
    let writer = std::thread::spawn(move || {
        let result = if writer_gate.wait_until_entered(Duration::from_secs(1)) {
            std::fs::write(writer_path, b"b")
        } else {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "evidence refresh did not reach its sampling gate",
            ))
        };
        writer_gate.release();
        result
    });

    assert_eq!(
        table.refresh_candidate_evidence(&mut queued).unwrap(),
        CandidateEvidenceRefresh::Retry
    );
    writer.join().unwrap().unwrap();
    assert_eq!(queued.evidence, original);
    table.shutdown().unwrap();
}

/// Scenario: discovery queues evidence for a short file, then the retained
/// descriptor observes a later append before the worker processes it.
/// Guarantees: the queued size and fingerprint are refreshed from the live
/// handle so stale sampling cannot be mistaken for truncation.
#[test]
fn queued_candidate_evidence_refreshes_from_resident_handle() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("queued.log");
    std::fs::write(&path, b"a").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 1)).unwrap();
    table.insert(candidate(&path), resolved(54, 0)).unwrap();
    let turn = data(table.poll(Instant::now()).unwrap());
    table
        .complete_turn(turn, 1, TurnDisposition::Paused)
        .unwrap();
    let mut queued = candidate(&path);
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"b")
        .unwrap();

    assert_eq!(
        table.refresh_candidate_evidence(&mut queued).unwrap(),
        CandidateEvidenceRefresh::Refreshed
    );
    assert_eq!(queued.evidence.size, 2);
    assert_eq!(queued.evidence.fingerprint, b"ab");
    table.shutdown().unwrap();
}

/// Scenario: copy-truncate is observed for a present reader whose descriptor
/// was evicted before the configured `read_new` reset.
/// Guarantees: preflight accepts the nonresident reader and the persisted
/// epoch/fingerprint state can reopen the same locator from offset zero.
#[test]
fn truncate_reset_reopens_present_nonresident_reader() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("nonresident-reset.log");
    std::fs::write(&path, b"old\n").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 4)).unwrap();
    table.insert(candidate(&path), resolved(55, 4)).unwrap();
    std::fs::write(&path, b"new\n").unwrap();

    table.preflight_truncate_reset(file_id(55), 0, 4).unwrap();
    table
        .apply_preflighted_truncate_reset(file_id(55), 0, 4, 1, b"new\n".to_vec(), true)
        .unwrap();
    let turn = data(table.poll(Instant::now()).unwrap());
    assert_eq!(turn.file_epoch(), 1);
    assert_eq!(turn.source_offset(), 0);
    assert_eq!(turn.bytes(), b"new\n");
    table
        .complete_turn(turn, 4, TurnDisposition::Paused)
        .unwrap();
    table.shutdown().unwrap();
}

/// Scenario: a removed reader's rotation deadline is earlier than its normal
/// EOF probe interval.
/// Guarantees: capping the scheduled probe makes the reader eligible at the
/// rotation deadline without extending or duplicating its EOF index.
#[test]
fn eof_probe_can_be_capped_at_rotation_deadline() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("capped-eof.log");
    std::fs::write(&path, b"a").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 1)).unwrap();
    table.insert(candidate(&path), resolved(56, 0)).unwrap();
    let now = Instant::now();
    let turn = data(table.poll(now).unwrap());
    table
        .complete_turn(turn, 1, TurnDisposition::Ready)
        .unwrap();
    let original = match table.poll(now).unwrap() {
        ReaderPoll::EndOfFile { next_probe, .. } => next_probe,
        other => panic!("expected EOF, got {other:?}"),
    };
    let capped = now.checked_add(Duration::from_millis(10)).unwrap();
    assert!(capped < original);
    table.cap_eof_deadline(file_id(56), capped).unwrap();
    assert!(matches!(
        table.poll(now).unwrap(),
        ReaderPoll::Idle {
            next_probe: Some(next)
        } if next == capped
    ));
    assert!(matches!(
        table.poll(capped).unwrap(),
        ReaderPoll::EndOfFile {
            file_id: observed,
            ..
        } if observed == file_id(56)
    ));
    table.shutdown().unwrap();
}

/// Scenario: identity revalidation fails for a still-present logical reader,
/// and later policy has already persisted a durable quarantine.
/// Guarantees: explicit revoke closes any descriptor, removes both indexes,
/// and releases the runtime lease without requiring a discovery removal.
#[test]
fn durable_revoke_releases_present_reader_lease() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("revoke.log");
    std::fs::write(&path, b"a").unwrap();
    let observed = candidate(&path);
    let locator = observed.evidence.locator;
    let mut table = ReaderTable::new(settings(1, 1, 1)).unwrap();
    table.insert(observed, resolved(26, 0)).unwrap();
    let mut other_scope = register_receiver_scope(1).unwrap();
    assert!(matches!(
        other_scope.try_acquire(locator),
        Err(LeaseError::Contended { .. })
    ));

    assert_eq!(table.release_revoked(file_id(26)).unwrap(), locator);
    assert_eq!(table.stats().tracked_readers, 0);
    let acquired = other_scope.try_acquire(locator).unwrap();
    acquired.release().unwrap();
    other_scope.close().unwrap();
    table.shutdown().unwrap();
}

/// Scenario: discovery refreshes path and fingerprint evidence while one
/// source turn from that reader is still being decoded.
/// Guarantees: the refresh is accepted atomically without duplicating the
/// ready entry or invalidating the outstanding turn.
#[test]
fn in_flight_candidate_update_is_atomic_and_does_not_requeue() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("updated.log");
    std::fs::write(&path, b"a").unwrap();
    let observed = candidate(&path);
    let mut table = ReaderTable::new(settings(1, 1, 1)).unwrap();
    table.insert(observed, resolved(27, 0)).unwrap();
    let now = Instant::now();

    let turn = data(table.poll(now).unwrap());
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"b")
        .unwrap();
    table.update(candidate(&path), &resolved(27, 0)).unwrap();
    assert_eq!(table.stats().ready_readers, 0);
    table
        .complete_turn(turn, 1, TurnDisposition::Paused)
        .unwrap();
    assert_eq!(table.stats().ready_readers, 0);
    table.make_ready(file_id(27)).unwrap();
    let appended = data(table.poll(now).unwrap());
    assert_eq!(appended.source_offset(), 1);
    assert_eq!(appended.bytes(), b"b");
    table
        .complete_turn(appended, 1, TurnDisposition::Paused)
        .unwrap();
    table.shutdown().unwrap();
}

/// Scenario: a split record's durable continuation state survives
/// temporary descriptor closure and later reopen.
/// Guarantees: every source turn exposes the same paired committed offset
/// and framing-resume state needed for deterministic Stage 9 replay.
#[test]
fn read_turn_exposes_continuation_frontier_across_reopen() {
    let directory = tempdir().unwrap();
    let first_path = directory.path().join("fragment.log");
    let second_path = directory.path().join("other.log");
    std::fs::write(&first_path, b"abcd").unwrap();
    std::fs::write(&second_path, b"z").unwrap();
    let resume = FramingResume::Continuation {
        record_start_offset: 0,
        record_end_offset: 0,
        next_fragment_index: 2,
    };
    let mut first_resolved = resolved_with_guard(28, 1, guard_for_prefix(b"a"));
    first_resolved.framing_resume = resume;
    let mut table = ReaderTable::new(settings(2, 1, 1)).unwrap();
    table
        .insert(candidate(&first_path), first_resolved)
        .unwrap();
    table
        .insert(candidate(&second_path), resolved(29, 0))
        .unwrap();
    let now = Instant::now();

    let first = data(table.poll(now).unwrap());
    assert_eq!(first.committed_offset(), 1);
    assert_eq!(first.framing_resume(), resume);
    table
        .complete_turn(first, 1, TurnDisposition::Ready)
        .unwrap();
    let request = eviction(table.poll(now).unwrap());
    table.confirm_eviction(request).unwrap();
    let second = data(table.poll(now).unwrap());
    table
        .complete_turn(second, 1, TurnDisposition::Paused)
        .unwrap();

    let request = eviction(table.poll(now).unwrap());
    table.confirm_eviction(request).unwrap();
    let replay = data(table.poll(now).unwrap());
    assert_eq!(replay.source_offset(), 1);
    assert_eq!(replay.committed_offset(), 1);
    assert_eq!(replay.framing_resume(), resume);
    table
        .complete_turn(replay, 1, TurnDisposition::Paused)
        .unwrap();
    table.shutdown().unwrap();
}

/// Scenario: a caller defers an LRS eviction because a batch is open, then
/// commits that batch and retries the descriptor-blocked target.
/// Guarantees: deferral joins the target to the batch-pause population, the
/// commit resumes it, and its fresh eviction request rejects the stale ticket.
#[test]
fn deferred_eviction_requires_fresh_request() {
    let directory = tempdir().unwrap();
    let first_path = directory.path().join("first.log");
    let second_path = directory.path().join("second.log");
    std::fs::write(&first_path, b"a").unwrap();
    std::fs::write(&second_path, b"b").unwrap();
    let mut table = ReaderTable::new(settings(2, 1, 1)).unwrap();
    table
        .insert(candidate(&first_path), resolved(30, 0))
        .unwrap();
    table
        .insert(candidate(&second_path), resolved(31, 0))
        .unwrap();
    let now = Instant::now();

    let first = data(table.poll(now).unwrap());
    table
        .complete_turn(first, 1, TurnDisposition::Ready)
        .unwrap();
    let original = eviction(table.poll(now).unwrap());
    assert!(matches!(
        table.pause(file_id(31)),
        Err(ReaderError::InvalidState { .. })
    ));
    table.defer_eviction(original).unwrap();
    assert!(matches!(
        table.confirm_eviction(original),
        Err(ReaderError::InvalidTurn { .. })
    ));
    assert!(
        table
            .frontier(file_id(31))
            .expect("target frontier exists")
            .paused_for_batch
    );
    table.finish_batch_commit(true).unwrap();
    assert!(
        !table
            .frontier(file_id(31))
            .expect("resumed target frontier exists")
            .paused_for_batch
    );
    let replacement = eviction(table.poll(now).unwrap());
    assert_ne!(replacement, original);
    table.confirm_eviction(replacement).unwrap();
    table.shutdown().unwrap();
}

/// Scenario: a temporarily closed path still names the same native file but
/// its durable fingerprint prefix was rewritten before reopen.
/// Guarantees: exact-locator equality alone cannot resume the old frontier;
/// the verified handle reports truncation before any rewritten byte is read.
#[test]
fn reopen_reports_same_locator_fingerprint_rewrite() {
    let directory = tempdir().unwrap();
    let first_path = directory.path().join("rewrite.log");
    let second_path = directory.path().join("other.log");
    std::fs::write(&first_path, b"abc").unwrap();
    std::fs::write(&second_path, b"z").unwrap();
    let mut table = ReaderTable::new(settings(2, 1, 1)).unwrap();
    table
        .insert(candidate(&first_path), resolved(34, 0))
        .unwrap();
    table
        .insert(candidate(&second_path), resolved(35, 0))
        .unwrap();
    let now = Instant::now();

    let first = data(table.poll(now).unwrap());
    table
        .complete_turn(first, 1, TurnDisposition::Ready)
        .unwrap();
    let request = eviction(table.poll(now).unwrap());
    table.confirm_eviction(request).unwrap();
    OpenOptions::new()
        .write(true)
        .open(&first_path)
        .unwrap()
        .write_all(b"x")
        .unwrap();
    let second = data(table.poll(now).unwrap());
    table
        .complete_turn(second, 1, TurnDisposition::Paused)
        .unwrap();

    let request = eviction(table.poll(now).unwrap());
    table.confirm_eviction(request).unwrap();
    assert!(matches!(
        table.poll(now).unwrap(),
        ReaderPoll::Truncated {
            file_id: rewritten,
            observed_size: 3,
            observed_fingerprint,
            ..
        } if rewritten == file_id(34) && observed_fingerprint == b"xbc"
    ));
    table.shutdown().unwrap();
}

/// Scenario: a temporarily closed same-locator source shrinks below its
/// committed offset while its stored fingerprint evidence is empty.
/// Guarantees: reopen independently enforces committed-offset <= current
/// size and reports truncation before reading from the stale frontier.
#[test]
fn reopen_reports_committed_offset_beyond_current_size() {
    let directory = tempdir().unwrap();
    let first_path = directory.path().join("shrink.log");
    let second_path = directory.path().join("other.log");
    std::fs::write(&first_path, b"a").unwrap();
    std::fs::write(&second_path, b"z").unwrap();
    let mut first_candidate = candidate(&first_path);
    first_candidate.evidence.fingerprint.clear();
    let mut table = ReaderTable::new(settings(2, 1, 1)).unwrap();
    table.insert(first_candidate, resolved(36, 1)).unwrap();
    table
        .insert(candidate(&second_path), resolved(37, 0))
        .unwrap();
    let now = Instant::now();

    assert!(matches!(
        table.poll(now).unwrap(),
        ReaderPoll::EndOfFile {
            file_id: eof_file,
            ..
        } if eof_file == file_id(36)
    ));
    let request = eviction(table.poll(now).unwrap());
    table.confirm_eviction(request).unwrap();
    OpenOptions::new()
        .write(true)
        .open(&first_path)
        .unwrap()
        .set_len(0)
        .unwrap();
    let second = data(table.poll(now).unwrap());
    table
        .complete_turn(second, 1, TurnDisposition::Paused)
        .unwrap();
    table.make_ready(file_id(36)).unwrap();

    let request = eviction(table.poll(now).unwrap());
    table.confirm_eviction(request).unwrap();
    assert!(matches!(
        table.poll(now).unwrap(),
        ReaderPoll::Truncated {
            file_id: truncated,
            committed_offset: 1,
            read_offset: 1,
            observed_size: 0,
            ..
        } if truncated == file_id(36)
    ));
    table.shutdown().unwrap();
}

/// Scenario: admission rejects inactive state and a population overflow
/// before a runtime lease could escape into the global registry.
/// Guarantees: each rejected candidate locator remains immediately
/// acquirable by another receiver scope.
#[test]
fn rejected_admission_does_not_leak_runtime_lease() {
    let directory = tempdir().unwrap();
    let first_path = directory.path().join("first.log");
    let second_path = directory.path().join("second.log");
    std::fs::write(&first_path, b"a").unwrap();
    std::fs::write(&second_path, b"b").unwrap();
    let first = candidate(&first_path);
    let first_locator = first.evidence.locator;
    let second = candidate(&second_path);
    let second_locator = second.evidence.locator;
    let mut table = ReaderTable::new(settings(1, 1, 1)).unwrap();
    let mut inactive = resolved(32, 0);
    inactive.lifecycle_state = LifecycleState::Quarantined;
    assert!(matches!(
        table.insert(first, inactive),
        Err(ReaderError::InactiveIdentity { .. })
    ));
    table
        .insert(candidate(&first_path), resolved(32, 0))
        .unwrap();
    assert!(matches!(
        table.insert(second, resolved(33, 0)),
        Err(ReaderError::ReaderCapacityExhausted { .. })
    ));

    let mut other_scope = register_receiver_scope(2).unwrap();
    assert!(matches!(
        other_scope.try_acquire(first_locator),
        Err(LeaseError::Contended { .. })
    ));
    let unleased = other_scope.try_acquire(second_locator).unwrap();
    unleased.release().unwrap();
    other_scope.close().unwrap();
    table.shutdown().unwrap();
}

/// Scenario: a caller pauses a reader for backpressure, then discovery
/// persists a valid same-locator path or fingerprint refresh.
/// Guarantees: evidence refresh does not override the explicit pause; only a
/// later `make_ready` resumes source reads.
#[test]
fn candidate_update_does_not_wake_explicitly_paused_reader() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("paused.log");
    std::fs::write(&path, b"a").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 1)).unwrap();
    table.insert(candidate(&path), resolved(38, 0)).unwrap();
    let now = Instant::now();

    let first = data(table.poll(now).unwrap());
    table
        .complete_turn(first, 1, TurnDisposition::Paused)
        .unwrap();
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"b")
        .unwrap();
    table.update(candidate(&path), &resolved(38, 0)).unwrap();
    assert!(matches!(table.poll(now).unwrap(), ReaderPoll::Idle { .. }));

    table.make_ready(file_id(38)).unwrap();
    let appended = data(table.poll(now).unwrap());
    assert_eq!(appended.source_offset(), 1);
    assert_eq!(appended.bytes(), b"b");
    table
        .complete_turn(appended, 1, TurnDisposition::Paused)
        .unwrap();
    table.shutdown().unwrap();
}

/// Scenario: two closed readers wait behind one pinned removed descriptor.
/// Guarantees: after the pinned reader is finalized, every waiter is
/// reconsidered and served through ordinary LRS rotation without another
/// discovery update.
#[test]
fn multiple_descriptor_waiters_eventually_rotate_through_one_slot() {
    let directory = tempdir().unwrap();
    let paths: Vec<_> = (39..=41)
        .map(|seed| {
            let path = directory.path().join(format!("{seed}.log"));
            std::fs::write(&path, [seed]).unwrap();
            path
        })
        .collect();
    let first = candidate(&paths[0]);
    let first_locator = first.evidence.locator;
    let mut table = ReaderTable::new(settings(3, 1, 1)).unwrap();
    table.insert(first, resolved(39, 0)).unwrap();
    table.insert(candidate(&paths[1]), resolved(40, 0)).unwrap();
    table.insert(candidate(&paths[2]), resolved(41, 0)).unwrap();
    let now = Instant::now();

    let resident = data(table.poll(now).unwrap());
    table
        .complete_turn(resident, 1, TurnDisposition::Ready)
        .unwrap();
    let _removed = table.mark_removed(first_locator).unwrap();
    assert!(matches!(
        table.poll(now).unwrap(),
        ReaderPoll::DescriptorCapacityBlocked { file_id: blocked }
            if blocked == file_id(40)
    ));
    assert_eq!(table.stats().descriptor_blocked_readers, 1);
    assert!(matches!(
        table.poll(now).unwrap(),
        ReaderPoll::DescriptorCapacityBlocked { file_id: blocked }
            if blocked == file_id(41)
    ));
    assert_eq!(table.stats().descriptor_blocked_readers, 2);
    assert!(matches!(
        table.poll(now).unwrap(),
        ReaderPoll::EndOfFile {
            file_id: eof_file,
            ..
        } if eof_file == file_id(39)
    ));
    let _released = table.release_finalized(file_id(39)).unwrap();
    assert_eq!(table.stats().descriptor_blocked_readers, 1);
    assert_eq!(table.stats().removed_readers, 0);
    assert_eq!(table.stats().open_files, 0);

    let second = data(table.poll(now).unwrap());
    assert_eq!(second.file_id(), file_id(40));
    table
        .complete_turn(second, 1, TurnDisposition::Paused)
        .unwrap();
    assert_eq!(table.stats().descriptor_blocked_readers, 0);
    let request = eviction(table.poll(now).unwrap());
    assert_eq!(request.target_file_id, file_id(41));
    assert_eq!(request.victim_file_id, file_id(40));
    table.confirm_eviction(request).unwrap();
    let third = data(table.poll(now).unwrap());
    assert_eq!(third.file_id(), file_id(41));
    table
        .complete_turn(third, 1, TurnDisposition::Paused)
        .unwrap();
    table.shutdown().unwrap();
}

/// Scenario: durable Ack progress advances an LRS victim after an eviction
/// request captured its older committed frontier.
/// Guarantees: the stale request is invalidated and the next poll produces a
/// fresh request reflecting the newly committed offset.
#[test]
fn committed_progress_refreshes_pending_eviction_request() {
    let directory = tempdir().unwrap();
    let first_path = directory.path().join("victim.log");
    let second_path = directory.path().join("target.log");
    std::fs::write(&first_path, b"a").unwrap();
    std::fs::write(&second_path, b"b").unwrap();
    let mut table = ReaderTable::new(settings(2, 1, 1)).unwrap();
    table
        .insert(candidate(&first_path), resolved(42, 0))
        .unwrap();
    table
        .insert(candidate(&second_path), resolved(43, 0))
        .unwrap();
    let now = Instant::now();

    let first = data(table.poll(now).unwrap());
    table
        .complete_turn(first, 1, TurnDisposition::Ready)
        .unwrap();
    let stale = eviction(table.poll(now).unwrap());
    table
        .observe_committed_progress(file_id(42), 0, 1, FramingResume::Clean)
        .unwrap();
    assert!(matches!(
        table.confirm_eviction(stale),
        Err(ReaderError::InvalidTurn { .. })
    ));
    let refreshed = eviction(table.poll(now).unwrap());
    assert_ne!(refreshed, stale);
    assert_eq!(refreshed.committed_offset, 1);
    assert_eq!(refreshed.read_offset, 1);
    table.confirm_eviction(refreshed).unwrap();
    table.shutdown().unwrap();
}

/// Scenario: a caller reports consuming more bytes than the bounded source
/// turn actually returned.
/// Guarantees: the invalid completion advances no offset, restores the sole
/// read buffer, pauses the reader, and permits an explicit deterministic
/// reread from the same frontier.
#[test]
fn invalid_turn_consumption_restores_buffer_and_frontier() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("invalid-consumption.log");
    std::fs::write(&path, b"a").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 1)).unwrap();
    table.insert(candidate(&path), resolved(44, 0)).unwrap();
    let now = Instant::now();

    let turn = data(table.poll(now).unwrap());
    assert!(matches!(
        table.complete_turn(turn, 2, TurnDisposition::Ready),
        Err(ReaderError::InvalidTurnConsumption {
            consumed: 2,
            available: 1,
            ..
        })
    ));
    assert!(matches!(table.poll(now).unwrap(), ReaderPoll::Idle { .. }));
    table.make_ready(file_id(44)).unwrap();
    let replay = data(table.poll(now).unwrap());
    assert_eq!(replay.source_offset(), 0);
    assert_eq!(replay.bytes(), b"a");
    table
        .complete_turn(replay, 1, TurnDisposition::Paused)
        .unwrap();
    table.shutdown().unwrap();
}

/// Scenario: a reader consumed six bytes, while a sealed provisional batch
/// owns only the first three.
/// Guarantees: rewind pauses at the supplied frontier, counts exactly three
/// replay bytes, preserves the resident descriptor, and permits a later Ack
/// observation followed by reading from that committed offset.
#[test]
fn provisional_frontier_rewind_preserves_descriptor_and_accepts_later_ack() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("provisional.log");
    std::fs::write(&path, b"abcdef").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 6)).unwrap();
    table.insert(candidate(&path), resolved(45, 0)).unwrap();
    let now = Instant::now();

    let turn = data(table.poll(now).unwrap());
    table
        .complete_turn(turn, 6, TurnDisposition::Paused)
        .unwrap();
    let before = table.stats();
    assert_eq!(before.open_files, 1);
    table
        .rewind_provisional_frontier(file_id(45), 0, 3)
        .unwrap();
    let after = table.stats();
    assert_eq!(after.open_files, 1);
    assert_eq!(after.opens, before.opens);
    assert_eq!(after.reopens, before.reopens);
    assert_eq!(after.source_bytes_rewound, 3);

    let resume = FramingResume::Continuation {
        record_start_offset: 1,
        record_end_offset: 0,
        next_fragment_index: 2,
    };
    table
        .observe_committed_progress(file_id(45), 0, 3, resume)
        .unwrap();
    table.resume_after_batch_commit().unwrap();
    let replay = data(table.poll(now).unwrap());
    assert_eq!(replay.committed_offset(), 3);
    assert_eq!(replay.framing_resume(), resume);
    assert_eq!(replay.source_offset(), 3);
    assert_eq!(replay.bytes(), b"def");
    table
        .complete_turn(replay, 3, TurnDisposition::Paused)
        .unwrap();
    table.shutdown().unwrap();
}

/// Scenario: provisional rewind supplies a stale epoch, an offset below the
/// durable frontier, and an offset beyond bytes consumed in memory.
/// Guarantees: every bounds/epoch failure leaves the reader frontier and
/// exact rewind counter unchanged.
#[test]
fn provisional_frontier_rewind_rejects_epoch_and_offset_bounds() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("bounds.log");
    std::fs::write(&path, b"abcdef").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 4)).unwrap();
    table
        .insert(
            candidate(&path),
            resolved_with_guard(46, 1, guard_for_prefix(b"a")),
        )
        .unwrap();
    let now = Instant::now();

    let turn = data(table.poll(now).unwrap());
    assert_eq!(turn.source_offset(), 1);
    table
        .complete_turn(turn, 3, TurnDisposition::Paused)
        .unwrap();
    assert!(matches!(
        table.rewind_provisional_frontier(file_id(46), 1, 2),
        Err(ReaderError::InvalidProgress { .. })
    ));
    assert!(matches!(
        table.rewind_provisional_frontier(file_id(46), 0, 0),
        Err(ReaderError::InvalidProgress { .. })
    ));
    assert!(matches!(
        table.rewind_provisional_frontier(file_id(46), 0, 5),
        Err(ReaderError::InvalidProgress { .. })
    ));
    assert_eq!(table.stats().source_bytes_rewound, 0);

    table.make_ready(file_id(46)).unwrap();
    let next = data(table.poll(now).unwrap());
    assert_eq!(next.source_offset(), 4);
    table
        .complete_turn(next, 0, TurnDisposition::Paused)
        .unwrap();
    table.shutdown().unwrap();
}

/// Scenario: provisional rewind is requested while a source turn is
/// outstanding and while a descriptor eviction involving that file awaits a
/// caller decision.
/// Guarantees: both conflicting scheduler states fail without changing
/// frontiers, descriptors, or the pending eviction contract.
#[test]
fn provisional_frontier_rewind_rejects_outstanding_turn_and_eviction() {
    let directory = tempdir().unwrap();
    let first_path = directory.path().join("first.log");
    let second_path = directory.path().join("second.log");
    std::fs::write(&first_path, b"a").unwrap();
    std::fs::write(&second_path, b"b").unwrap();
    let mut table = ReaderTable::new(settings(2, 1, 1)).unwrap();
    table
        .insert(candidate(&first_path), resolved(47, 0))
        .unwrap();
    table
        .insert(candidate(&second_path), resolved(48, 0))
        .unwrap();
    let now = Instant::now();

    let first = data(table.poll(now).unwrap());
    assert!(matches!(
        table.rewind_provisional_frontier(file_id(47), 0, 0),
        Err(ReaderError::InvalidState { .. })
    ));
    table
        .complete_turn(first, 1, TurnDisposition::Ready)
        .unwrap();
    let request = eviction(table.poll(now).unwrap());
    assert!(matches!(
        table.rewind_provisional_frontier(request.victim_file_id, 0, 1),
        Err(ReaderError::InvalidState { .. })
    ));
    assert!(matches!(
        table.rewind_provisional_frontier(request.target_file_id, 0, 0),
        Err(ReaderError::InvalidState { .. })
    ));
    table.defer_eviction(request).unwrap();
    table.shutdown().unwrap();
}

/// Scenario: two bounded readers expose different durable and provisional
/// frontiers after only one consumes source bytes.
/// Guarantees: allocation-free frontier iteration reports every reader
/// exactly once with exact epoch, offsets, presence, descriptor, and
/// batch-pause state.
#[test]
fn frontier_iteration_reports_exact_bounded_reader_state() {
    let directory = tempdir().unwrap();
    let first_path = directory.path().join("first.log");
    let second_path = directory.path().join("second.log");
    std::fs::write(&first_path, b"abcd").unwrap();
    std::fs::write(&second_path, b"wxyz").unwrap();
    let mut table = ReaderTable::new(settings(2, 2, 4)).unwrap();
    table
        .insert(candidate(&first_path), resolved(49, 0))
        .unwrap();
    table
        .insert(candidate(&second_path), resolved(50, 1))
        .unwrap();

    let turn = data(table.poll(Instant::now()).unwrap());
    assert_eq!(turn.file_id(), file_id(49));
    table
        .complete_turn(turn, 3, TurnDisposition::Paused)
        .unwrap();
    table
        .rewind_provisional_frontier(file_id(49), 0, 2)
        .unwrap();

    let mut frontiers: Vec<_> = table.frontiers().collect();
    frontiers.sort_unstable_by_key(|frontier| frontier.file_id);
    assert_eq!(
        frontiers,
        vec![
            ReaderFrontier {
                file_id: file_id(49),
                file_epoch: 0,
                committed_offset: 0,
                read_offset: 2,
                framing_resume: FramingResume::Clean,
                present: true,
                descriptor_resident: true,
                paused_for_batch: true,
            },
            ReaderFrontier {
                file_id: file_id(50),
                file_epoch: 0,
                committed_offset: 1,
                read_offset: 1,
                framing_resume: FramingResume::Clean,
                present: true,
                descriptor_resident: false,
                paused_for_batch: false,
            },
        ]
    );
    table.shutdown().unwrap();
}

/// Scenario: a reader owns matched and resolved paths until a removed
/// identity is genuinely released.
/// Guarantees: record context borrows the exact reader paths without a
/// secondary map, and a stale lookup fails after release frees the bounded
/// reader slot.
#[test]
fn record_context_is_exact_and_stale_after_reader_release() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("context.log");
    std::fs::write(&path, b"a").unwrap();
    let observed = candidate(&path);
    let expected_resolved = observed.resolved_path.clone();
    let locator = observed.evidence.locator;
    let mut table = ReaderTable::new(settings(1, 1, 1)).unwrap();
    table.insert(observed, resolved(51, 0)).unwrap();

    let context = table.record_context(file_id(51)).unwrap();
    assert_eq!(context.matched_path, path);
    assert_eq!(context.resolved_path, expected_resolved);
    let identity = table.identity_context(locator).unwrap();
    assert_eq!(identity.file_id, file_id(51));
    assert_eq!(identity.file_epoch, 0);
    assert_eq!(identity.committed_offset, 0);
    assert_eq!(identity.durable_fingerprint, b"a");
    assert_eq!(
        table.mark_removed(locator).unwrap(),
        RemovalDisposition::DescriptorAbsent
    );
    assert_eq!(table.release_finalized(file_id(51)).unwrap(), locator);
    assert!(matches!(
        table.record_context(file_id(51)),
        Err(ReaderError::UnknownFile { .. })
    ));

    table.shutdown().unwrap();
}

/// Scenario: drain replay captures a source frontier three bytes into a
/// six-byte file.
/// Guarantees: the reader-owned bounded poll returns exactly the captured
/// prefix and never admits bytes beyond that drain frontier.
#[test]
fn bounded_drain_poll_stops_at_captured_frontier() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("drain-limit.log");
    std::fs::write(&path, b"abcdef").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 6)).unwrap();
    table.insert(candidate(&path), resolved(52, 0)).unwrap();
    table.pause(file_id(52)).unwrap();

    let turn = data(table.poll_until(Instant::now(), file_id(52), 3).unwrap());
    assert_eq!(turn.source_offset(), 0);
    assert_eq!(turn.bytes(), b"abc");
    table
        .complete_turn(turn, 3, TurnDisposition::Paused)
        .unwrap();
    assert_eq!(table.frontier(file_id(52)).unwrap().read_offset, 3);
    table.shutdown().unwrap();
}

/// Scenario: a sealed reader population is Ack-committed while receiver drain
/// still has bounded replay work.
/// Guarantees: clearing batch-pause markers without resume leaves the reader
/// paused until drain explicitly selects it.
#[test]
fn batch_commit_can_preserve_pause_for_drain_replay() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("drain-paused.log");
    std::fs::write(&path, b"abc").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 3)).unwrap();
    table.insert(candidate(&path), resolved(53, 0)).unwrap();
    let turn = data(table.poll(Instant::now()).unwrap());
    table
        .complete_turn(turn, 3, TurnDisposition::Paused)
        .unwrap();
    table
        .rewind_provisional_frontier(file_id(53), 0, 1)
        .unwrap();
    table
        .observe_committed_progress(file_id(53), 0, 1, FramingResume::Clean)
        .unwrap();
    table.finish_batch_commit(false).unwrap();

    assert!(matches!(
        table.poll(Instant::now()).unwrap(),
        ReaderPoll::Idle { .. }
    ));
    let replay = data(table.poll_until(Instant::now(), file_id(53), 3).unwrap());
    assert_eq!(replay.source_offset(), 1);
    assert_eq!(replay.bytes(), b"bc");
    table
        .complete_turn(replay, 2, TurnDisposition::Paused)
        .unwrap();
    table.shutdown().unwrap();
}

/// Scenario: the committed-frontier window accessor is called repeatedly
/// after the descriptor has already served a real read that established it.
/// Guarantees: every call is a bounded in-memory clone of reader-owned
/// bytes; it never issues another positioned source read, eliminating
/// post-Ack rereads.
#[test]
fn committed_frontier_window_accessor_never_rereads_the_source() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("no-reread.log");
    std::fs::write(&path, b"abcxyz").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 6)).unwrap();
    table
        .insert(
            candidate(&path),
            resolved_with_guard(70, 3, guard_for_prefix(b"abc")),
        )
        .unwrap();
    let now = Instant::now();

    let turn = data(table.poll(now).unwrap());
    assert_eq!(turn.source_offset(), 3);
    table
        .complete_turn(turn, 3, TurnDisposition::Paused)
        .unwrap();
    assert_eq!(
        table
            .committed_frontier_window(file_id(70), 3)
            .unwrap()
            .bytes(),
        b"abc"
    );
    let baseline = table.stats();

    for _ in 0..5 {
        let window = table.committed_frontier_window(file_id(70), 3).unwrap();
        assert_eq!(window.bytes(), b"abc");
    }

    let after = table.stats();
    assert_eq!(after.read_turns, baseline.read_turns);
    assert_eq!(after.source_bytes_read, baseline.source_bytes_read);
    assert_eq!(after.opens, baseline.opens);
    assert_eq!(after.reopens, baseline.reopens);
    table.shutdown().unwrap();
}

/// Scenario: a resident descriptor with an established committed-frontier
/// window is evicted and later reopened while its content is unchanged.
/// Guarantees: reopen independently re-validates the retained window
/// against the durable guard and refreshes it from the newly (re)opened
/// handle rather than blindly trusting stale bytes, and the refreshed
/// window is byte-for-byte the same real evidence.
#[test]
fn descriptor_reopen_revalidates_and_refreshes_committed_frontier_window() {
    let directory = tempdir().unwrap();
    let first_path = directory.path().join("evict-revalidate.log");
    let second_path = directory.path().join("other.log");
    std::fs::write(&first_path, b"abcxyz").unwrap();
    std::fs::write(&second_path, b"z").unwrap();
    let mut table = ReaderTable::new(settings(2, 1, 6)).unwrap();
    table
        .insert(
            candidate(&first_path),
            resolved_with_guard(71, 3, guard_for_prefix(b"abc")),
        )
        .unwrap();
    table
        .insert(candidate(&second_path), resolved(72, 0))
        .unwrap();
    let now = Instant::now();

    let first = data(table.poll(now).unwrap());
    assert_eq!(first.file_id(), file_id(71));
    table
        .complete_turn(first, 3, TurnDisposition::Paused)
        .unwrap();
    assert_eq!(
        table
            .committed_frontier_window(file_id(71), 3)
            .unwrap()
            .bytes(),
        b"abc"
    );

    table.make_ready(file_id(72)).unwrap();
    let request = eviction(table.poll(now).unwrap());
    assert_eq!(request.victim_file_id, file_id(71));
    table.confirm_eviction(request).unwrap();

    let second = data(table.poll(now).unwrap());
    assert_eq!(second.file_id(), file_id(72));
    table
        .complete_turn(second, 1, TurnDisposition::Paused)
        .unwrap();

    table.make_ready(file_id(71)).unwrap();
    let request = eviction(table.poll(now).unwrap());
    assert_eq!(request.victim_file_id, file_id(72));
    table.confirm_eviction(request).unwrap();

    let reopened = data(table.poll(now).unwrap());
    assert_eq!(reopened.file_id(), file_id(71));
    assert_eq!(reopened.source_offset(), 3);
    table
        .complete_turn(reopened, 3, TurnDisposition::Paused)
        .unwrap();
    assert_eq!(
        table
            .committed_frontier_window(file_id(71), 3)
            .unwrap()
            .bytes(),
        b"abc"
    );
    table.shutdown().unwrap();
}

/// Scenario: a resident descriptor is evicted, then bytes strictly inside
/// its committed-frontier window (but past the durable fingerprint prefix)
/// change on disk before reopen, while the file's size and fingerprint
/// prefix stay identical.
/// Guarantees: reopen classifies the mismatch through the same
/// unavailable-reopen path as an incompatible fingerprint or locator; the
/// reader is never allowed to silently resume from evidence that no longer
/// matches what was durably recorded.
#[test]
fn descriptor_reopen_fails_closed_on_committed_frontier_guard_mismatch() {
    let directory = tempdir().unwrap();
    let first_path = directory.path().join("guard-mismatch.log");
    let second_path = directory.path().join("other.log");
    std::fs::write(&first_path, b"abcxyz").unwrap();
    std::fs::write(&second_path, b"z").unwrap();
    let mut table = ReaderTable::new(settings_with_fingerprint(2, 1, 6, 1)).unwrap();
    table
        .insert(
            candidate_with_fingerprint(&first_path, 1),
            resolved_with_guard(73, 3, guard_for_prefix(b"abc")),
        )
        .unwrap();
    table
        .insert(candidate_with_fingerprint(&second_path, 1), resolved(74, 0))
        .unwrap();
    let now = Instant::now();

    let first = data(table.poll(now).unwrap());
    assert_eq!(first.file_id(), file_id(73));
    table
        .complete_turn(first, 3, TurnDisposition::Paused)
        .unwrap();

    table.make_ready(file_id(74)).unwrap();
    let request = eviction(table.poll(now).unwrap());
    assert_eq!(request.victim_file_id, file_id(73));
    table.confirm_eviction(request).unwrap();

    // The 1-byte fingerprint prefix ("a") and the 6-byte size are both
    // unchanged, so reopen's fingerprint/size check alone would accept
    // this candidate; only committed-frontier guard validation can detect
    // that the retained evidence no longer matches.
    std::fs::write(&first_path, b"aXcxyz").unwrap();

    let second = data(table.poll(now).unwrap());
    assert_eq!(second.file_id(), file_id(74));
    table
        .complete_turn(second, 1, TurnDisposition::Paused)
        .unwrap();

    table.make_ready(file_id(73)).unwrap();
    let request = eviction(table.poll(now).unwrap());
    assert_eq!(request.victim_file_id, file_id(74));
    table.confirm_eviction(request).unwrap();

    assert!(matches!(
        table.poll(now).unwrap(),
        ReaderPoll::RemovedWithoutDescriptor {
            file_id: unavailable
        } if unavailable == file_id(73)
    ));
    assert!(!table.frontier(file_id(73)).unwrap().present);
    table.shutdown().unwrap();
}

/// Scenario: a genuinely new identity admission supplies a durable guard
/// that does not match the candidate's real committed-frontier evidence at
/// a nonzero, mid-file offset (the restart/recovery shape: an existing
/// identity resuming below its current size).
/// Guarantees: the very first open independently validates against the
/// supplied guard and fails closed exactly like a later reopen mismatch.
#[test]
fn restart_admission_fails_closed_on_wrong_committed_frontier_guard() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("restart-mismatch.log");
    std::fs::write(&path, b"abcxyz").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 6)).unwrap();
    table
        .insert(
            candidate(&path),
            resolved_with_guard(75, 3, guard_for_prefix(b"zzz")),
        )
        .unwrap();

    assert!(matches!(
        table.poll(Instant::now()).unwrap(),
        ReaderPoll::RemovedWithoutDescriptor {
            file_id: unavailable
        } if unavailable == file_id(75)
    ));
    assert!(!table.frontier(file_id(75)).unwrap().present);
    table.shutdown().unwrap();
}

/// Scenario: a durably recorded fingerprint prefix no longer matches the
/// candidate's current bytes, even though the exact committed-frontier
/// window bytes at the stored committed offset (a separate, tail-only
/// region of a larger file) are byte-for-byte unchanged and would pass
/// guard validation on their own.
/// Guarantees: prefix evidence is checked -- and fails closed -- before the
/// committed-frontier window is ever read; a matching tail window never
/// overrides a mismatched configured-prefix evidence.
#[test]
fn reopen_reports_fingerprint_mismatch_even_when_frontier_window_matches() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("prefix-mismatch-matching-window.log");
    let original: Vec<u8> = (0..100u32).map(|i| b'a' + (i % 26) as u8).collect();
    std::fs::write(&path, &original).unwrap();
    // The durable fingerprint is captured from the original prefix before
    // the file is rewritten below.
    let candidate = candidate_with_fingerprint(&path, 4);
    assert_eq!(candidate.evidence.fingerprint, original[..4]);
    // The real committed-frontier window ending at offset 90 is the tail
    // region [26, 90), entirely disjoint from the rewritten prefix [0, 4).
    let committed_offset = 90u64;
    let window_start = (committed_offset - 64) as usize;
    let guard =
        CommittedFrontierGuard::compute(committed_offset, &original[window_start..90]).unwrap();
    // Only the prefix changes; the tail window bytes are left identical.
    let mut rewritten = original.clone();
    rewritten[..4].copy_from_slice(b"ZZZZ");
    std::fs::write(&path, &rewritten).unwrap();

    let mut table = ReaderTable::new(settings_with_fingerprint(1, 1, 100, 4)).unwrap();
    table
        .insert(candidate, resolved_with_guard(77, committed_offset, guard))
        .unwrap();

    assert!(matches!(
        table.poll(Instant::now()).unwrap(),
        ReaderPoll::Truncated {
            file_id: mismatched,
            observed_fingerprint,
            ..
        } if mismatched == file_id(77) && observed_fingerprint == b"ZZZZ"
    ));
    table.shutdown().unwrap();
}

/// Scenario: a byte strictly between the sampled fingerprint prefix and the
/// sampled committed-frontier window -- a region this design never reads
/// for matching -- changes on disk while the prefix and the tail window
/// are both byte-for-byte unchanged.
/// Guarantees: this is the design's explicit, accepted residual ambiguity,
/// not a detection gap to close: bounded-evidence identity matching
/// resumes normally (no truncation, no error) because the unchanged
/// sampled evidence is, by construction, the only evidence this design
/// ever checks. It never claims to detect an in-place rewrite confined to
/// unsampled middle bytes.
#[test]
fn unchecked_middle_byte_rewrite_is_the_accepted_residual_ambiguity() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("middle-byte-residual-ambiguity.log");
    let original: Vec<u8> = (0..200u32).map(|i| b'a' + (i % 26) as u8).collect();
    std::fs::write(&path, &original).unwrap();
    let candidate = candidate_with_fingerprint(&path, 4);
    assert_eq!(candidate.evidence.fingerprint, original[..4]);
    let committed_offset = 180u64;
    let window_start = (committed_offset - 64) as usize; // 116
    let guard =
        CommittedFrontierGuard::compute(committed_offset, &original[window_start..180]).unwrap();

    // A middle byte, strictly outside both the 4-byte prefix [0, 4) and the
    // 64-byte tail window [116, 180), changes on disk.
    let middle_index = 50usize;
    assert!(middle_index >= 4 && middle_index < window_start);
    let mut rewritten = original.clone();
    rewritten[middle_index] = rewritten[middle_index].wrapping_add(1);
    std::fs::write(&path, &rewritten).unwrap();

    let mut table = ReaderTable::new(settings_with_fingerprint(1, 1, 200, 4)).unwrap();
    table
        .insert(candidate, resolved_with_guard(78, committed_offset, guard))
        .unwrap();

    // The mismatch is invisible to bounded-evidence matching: the reader
    // resumes normally rather than reporting truncation or an error.
    let turn = data(table.poll(Instant::now()).unwrap());
    assert_eq!(turn.source_offset(), committed_offset);
    table.shutdown().unwrap();
}

/// Scenario: discovery captures a new identity at EOF, then the same
/// locator is rewritten behind an unchanged fingerprint prefix before the
/// reader's separate first open.
/// Guarantees: the first nonzero open revalidates the durable frontier guard
/// against its own handle and cannot trust stale discovery-window bytes.
#[test]
fn first_nonzero_open_revalidates_new_identity_frontier_guard() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("new-identity-guard-race.log");
    std::fs::write(&path, b"abc").unwrap();
    let candidate = candidate_with_fingerprint(&path, 1);
    let guard = candidate
        .evidence
        .committed_frontier_window
        .guard()
        .unwrap();
    std::fs::write(&path, b"axy").unwrap();
    let mut table = ReaderTable::new(settings_with_fingerprint(1, 1, 3, 1)).unwrap();
    table
        .insert(candidate, resolved_with_guard(76, 3, guard))
        .unwrap();

    assert!(matches!(
        table.poll(Instant::now()).unwrap(),
        ReaderPoll::RemovedWithoutDescriptor {
            file_id: unavailable
        } if unavailable == file_id(76)
    ));
    assert!(!table.frontier(file_id(76)).unwrap().present);
    table.shutdown().unwrap();
}

/// Scenario: a reader with a real, nonzero committed-frontier window is
/// reset after truncation.
/// Guarantees: the retained window becomes exactly the empty window (never
/// a stale nonzero-offset window), matching the reset offset of zero.
#[test]
fn truncate_reset_installs_empty_committed_frontier_window() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("truncate-empty-window.log");
    std::fs::write(&path, b"old\n").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 4)).unwrap();
    table.insert(candidate(&path), resolved(80, 4)).unwrap();
    assert_eq!(
        table
            .committed_frontier_window(file_id(80), 4)
            .unwrap()
            .bytes(),
        b"old\n"
    );

    std::fs::write(&path, b"new\n").unwrap();
    table.preflight_truncate_reset(file_id(80), 0, 4).unwrap();
    table
        .apply_preflighted_truncate_reset(file_id(80), 0, 4, 1, b"new\n".to_vec(), false)
        .unwrap();

    let window = table.committed_frontier_window(file_id(80), 0).unwrap();
    assert_eq!(window, CommittedFrontierWindow::empty());
    table.shutdown().unwrap();
}

/// Scenario: an Ack-resulting window is installed for the current committed
/// offset, then an out-of-band window ending elsewhere is attempted.
/// Guarantees: a matching offset replaces the retained bytes and the
/// derived guard together; a mismatched offset is rejected without
/// mutating either.
#[test]
fn install_committed_frontier_window_requires_matching_offset() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("install-window.log");
    std::fs::write(&path, b"abcxyz").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 6)).unwrap();
    table.insert(candidate(&path), resolved(81, 0)).unwrap();

    let window = CommittedFrontierWindow::new(0, Vec::new()).unwrap();
    let wrong_offset = CommittedFrontierWindow::new(3, b"abc".to_vec()).unwrap();
    assert!(matches!(
        table.install_committed_frontier_window(file_id(81), Some(wrong_offset)),
        Err(ReaderError::Inconsistent { .. })
    ));
    assert_eq!(
        table.committed_frontier_window(file_id(81), 0).unwrap(),
        window
    );

    // `None` (a zero-delta or finalize-only update) is always accepted and
    // never mutates the retained window.
    table
        .install_committed_frontier_window(file_id(81), None)
        .unwrap();
    assert_eq!(
        table.committed_frontier_window(file_id(81), 0).unwrap(),
        window
    );
    table.shutdown().unwrap();
}

/// Scenario: a reader resumes a durable continuation whose exact known
/// record end lies beyond the reopened handle's observed size, even though
/// the size still covers the committed offset itself.
/// Guarantees: reopen classifies this as truncation before any
/// continuation bytes are emitted, exactly like a source shorter than
/// bytes already consumed, rather than silently reinterpreting the
/// deterministic boundary under scan-to-next-LF semantics.
#[test]
fn reopen_reports_truncation_when_size_is_below_known_continuation_end() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("continuation-truncated.log");
    std::fs::write(&path, b"abcdef").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 6)).unwrap();
    let mut identity = resolved(90, 4);
    identity.framing_resume = FramingResume::Continuation {
        record_start_offset: 0,
        record_end_offset: 20,
        next_fragment_index: 1,
    };
    table.insert(candidate(&path), identity).unwrap();

    assert!(matches!(
        table.poll(Instant::now()).unwrap(),
        ReaderPoll::Truncated {
            committed_offset: 4,
            observed_size: 6,
            ..
        }
    ));
    table.shutdown().unwrap();
}

/// Scenario: a resident continuation with known end 10 reads through offset
/// 7, then the source is truncated to that temporary EOF.
/// Guarantees: EOF metadata revalidation reports truncation because the
/// durable record end is missing, rather than allowing D17 to emit a false
/// final fragment.
#[test]
fn resident_eof_reports_truncation_below_known_continuation_end() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("continuation-live-truncated.log");
    std::fs::write(&path, b"abcdefghij").unwrap();
    let mut table = ReaderTable::new(settings(1, 1, 10)).unwrap();
    let mut identity = resolved_with_guard(91, 4, guard_for_prefix(b"abcd"));
    identity.framing_resume = FramingResume::Continuation {
        record_start_offset: 0,
        record_end_offset: 10,
        next_fragment_index: 1,
    };
    table.insert(candidate(&path), identity).unwrap();

    let turn = data(table.poll(Instant::now()).unwrap());
    assert_eq!(turn.source_offset(), 4);
    table
        .complete_turn(turn, 3, TurnDisposition::Ready)
        .unwrap();
    OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(7)
        .unwrap();

    assert!(matches!(
        table.poll(Instant::now()).unwrap(),
        ReaderPoll::Truncated {
            committed_offset: 4,
            read_offset: 7,
            observed_size: 7,
            ..
        }
    ));
    table.shutdown().unwrap();
}
