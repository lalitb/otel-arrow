// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use tempfile::tempdir;

use super::*;
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

fn file_id(seed: u8) -> FileId {
    FileId::from_bytes([seed; 16])
}

fn candidate(path: &Path) -> DiscoveredCandidate {
    let resolved_path = std::fs::canonicalize(path).expect("candidate canonicalizes");
    let opened = open_candidate(&resolved_path, false, 16, 0).expect("candidate opens");
    DiscoveredCandidate {
        matched_path: path.to_path_buf(),
        resolved_path,
        evidence: opened.evidence,
        modified: None,
    }
}

fn resolved(seed: u8, offset: u64) -> ResolvedIdentity {
    ResolvedIdentity {
        file_id: file_id(seed),
        file_epoch: 0,
        committed_offset: offset,
        framing_resume: FramingResume::Clean,
        lifecycle_state: LifecycleState::Active,
        matched_by: IdentityMatch::NewDiscovery,
    }
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
    assert_eq!(
        table.mark_removed(locator).unwrap(),
        RemovalDisposition::HandleRetained
    );
    assert!(matches!(
        table.poll(now).unwrap(),
        ReaderPoll::DescriptorCapacityBlocked { file_id: blocked } if blocked == file_id(8)
    ));
    assert_eq!(table.stats().open_files, 1);
    assert_eq!(table.stats().removed_readers, 1);

    table.pause(file_id(7)).unwrap();
    let _released = table.release_finalized(file_id(7)).unwrap();
    let replacement = data(table.poll(now).unwrap());
    assert_eq!(replacement.file_id(), file_id(8));
    table
        .complete_turn(replacement, 1, TurnDisposition::Paused)
        .unwrap();
    table.shutdown().unwrap();
}

/// Scenario: a closed logical reader's path is replaced by a different
/// native file before the reader is scheduled again.
/// Guarantees: reopen identity validation rejects the replacement before any
/// source byte is read under the old durable identity.
#[test]
fn reopen_rejects_replacement_locator_before_reading() {
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
    let error = table
        .poll(now)
        .expect_err("replacement locator must fail reopen");
    assert!(matches!(
        error,
        ReaderError::Reopen {
            source: IdentityError::ReopenLocatorMismatch { .. },
            ..
        }
    ));
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
    table.insert(candidate(&path), resolved(15, 1)).unwrap();
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
        next_fragment_index: 2,
    };
    let mut first_resolved = resolved(28, 1);
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

/// Scenario: a caller defers an LRS eviction until an external batch
/// boundary, then retries the target.
/// Guarantees: deferral pauses the target, stale confirmation is rejected,
/// and the later request receives a fresh correlation ticket.
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
    table.pause(file_id(30)).unwrap();
    table.make_ready(file_id(31)).unwrap();
    let replacement = eviction(table.poll(now).unwrap());
    assert_ne!(replacement, original);
    table.confirm_eviction(replacement).unwrap();
    table.shutdown().unwrap();
}

/// Scenario: a temporarily closed path still names the same native file but
/// its durable fingerprint prefix was rewritten before reopen.
/// Guarantees: exact-locator equality alone cannot resume the old reader;
/// fingerprint mismatch is reported before any rewritten byte is read.
#[test]
fn reopen_rejects_same_locator_fingerprint_rewrite() {
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
        table.poll(now),
        Err(ReaderError::Reopen {
            source: IdentityError::ReopenFingerprintMismatch { .. },
            ..
        })
    ));
    table.shutdown().unwrap();
}

/// Scenario: a temporarily closed same-locator source shrinks below its
/// committed offset while its stored fingerprint evidence is empty.
/// Guarantees: reopen independently enforces committed-offset <= current
/// size and refuses to read from the stale durable frontier.
#[test]
fn reopen_rejects_committed_offset_beyond_current_size() {
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
        table.poll(now),
        Err(ReaderError::Reopen {
            source: IdentityError::ReopenOffsetBeyondSize {
                committed_offset: 1,
                size: 0,
                ..
            },
            ..
        })
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
    assert!(matches!(
        table.poll(now).unwrap(),
        ReaderPoll::DescriptorCapacityBlocked { file_id: blocked }
            if blocked == file_id(41)
    ));
    assert!(matches!(
        table.poll(now).unwrap(),
        ReaderPoll::EndOfFile {
            file_id: eof_file,
            ..
        } if eof_file == file_id(39)
    ));
    let _released = table.release_finalized(file_id(39)).unwrap();

    let second = data(table.poll(now).unwrap());
    assert_eq!(second.file_id(), file_id(40));
    table
        .complete_turn(second, 1, TurnDisposition::Paused)
        .unwrap();
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
