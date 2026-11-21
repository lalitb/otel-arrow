// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

use super::*;
use crate::receivers::filelog_receiver::{
    config::Rotation,
    discovery::{
        Observation, ScanCompletion, ScanReason,
        reconcile::{BindingRole, ReconcileLimits, Reconciler, TrackedFile, TrackedId},
    },
};

const ID: TrackedId = TrackedId(7);

struct Fixture {
    readers: Scheduler<FakeIo>,
    sink: Sink,
    scan: Reconciler,
    generation: u64,
    observations: u64,
    clock: Rc<Cell<Instant>>,
    rotation: Rotation,
}

impl Fixture {
    fn at_eof() -> Self {
        Self::with_file(b"", Duration::from_millis(250))
    }

    fn with_file(bytes: &[u8], reprobe_interval: Duration) -> Self {
        let mut readers = scheduler(&[bytes], 64, 32, 1, 1, FramingConfig::Line(line()));
        readers.limits.eof_interval = reprobe_interval;
        let mut sink = Sink::default();
        let now = Instant::now();
        let clock = Rc::new(Cell::new(now));
        let eof_clock = Rc::clone(&clock);
        readers.eof_clock = Box::new(move || eof_clock.get());
        park_at_eof(&mut readers, &mut sink, 1, now);
        let mut scan = Reconciler::new(ReconcileLimits {
            tracked: 1,
            pending: 1,
            offers: 1,
            path_bytes: 1024,
            fingerprint_bytes: 16,
        })
        .unwrap();
        scan.track(TrackedFile {
            id: ID,
            locator: readers.io.files[0].locator,
            binding: Some("/logs/app.log".into()),
            role: BindingRole::Live,
            active: true,
            resident: true,
        })
        .unwrap();
        Self {
            readers,
            sink,
            scan,
            generation: 0,
            observations: 0,
            clock,
            rotation: Rotation::default(),
        }
    }

    fn begin(&mut self, start: u64) {
        self.generation += 1;
        self.observations = 0;
        self.scan
            .begin(self.generation, Duration::from_millis(start), 0)
            .unwrap();
    }

    fn finish(&mut self, end: u64) {
        let reader = self.readers.disappearance(ReaderKey(0, 0)).unwrap();
        let _ = self
            .scan
            .finish(
                ScanCompletion {
                    generation: self.generation,
                    observations: self.observations,
                    complete: true,
                    cancelled: false,
                },
                Duration::from_millis(end),
                |event| reader.observe(ID, &event),
            )
            .unwrap();
    }

    fn miss(&mut self, start: u64, end: u64) {
        self.begin(start);
        self.finish(end);
    }

    fn observe(&mut self, at: u64, reason: Option<ScanReason>) {
        let file = &self.readers.io.files[0];
        self.feed(
            at,
            Observation {
                path: "/logs/app.log".into(),
                locator: Some(file.locator),
                size: reason.is_none().then_some(file.bytes.len() as u64),
                modified: None,
                fingerprint: Vec::new(),
                reason,
                errno: None,
            },
        );
    }

    fn feed(&mut self, at: u64, observation: Observation) {
        let reader = self.readers.disappearance(ReaderKey(0, 0)).unwrap();
        self.scan
            .observe(
                self.generation,
                &observation,
                Duration::from_millis(at),
                |event| reader.observe(ID, &event),
            )
            .unwrap();
        self.observations += 1;
    }

    fn turn_at(&mut self, at: Instant) -> Reason {
        self.clock.set(at);
        tick(&mut self.readers, &mut self.sink, at).reason
    }

    fn met(&mut self) -> bool {
        self.readers
            .disappearance(ReaderKey(0, 0))
            .unwrap()
            .discovery_and_eof_inactivity_met(&self.rotation)
    }
}

/// Scenario: EOF inactivity expires with only one miss or a pass that started exactly at its finish.
/// Guarantees: Only a complete miss started strictly later satisfies the outstanding discovery condition.
#[test]
fn disappearance_requires_a_later_pass_start() {
    let mut f = Fixture::at_eof();
    let expired = f.clock.get() + f.rotation.rotate_wait;
    assert!(matches!(f.turn_at(expired), Reason::Eof));
    f.miss(0, 10);
    assert!(!f.met());
    f.miss(10, 20);
    assert!(!f.met());
    f.miss(21, 30);
    assert!(f.met());
}

/// Scenario: Discovery confirms before the deadline, then the resident file grows just before expiry.
/// Guarantees: The appended line is delivered to the sink and only a full interval after the new EOF satisfies inactivity.
#[test]
fn disappearance_late_line_restarts_only_eof_inactivity() {
    let mut f = Fixture::at_eof();
    let expiry = f.clock.get() + f.rotation.rotate_wait;
    let append = expiry - Duration::from_millis(1);
    f.miss(0, 1);
    f.miss(2, 3);
    assert!(!f.met());
    f.clock.set(append);
    f.readers.io.files[0].bytes.extend_from_slice(b"late\n");

    f.sink.blocked = true;
    assert!(matches!(f.turn_at(expiry), Reason::Paused));
    assert!(!f.met());
    f.sink.blocked = false;
    f.readers.resume_sink();
    frames(&mut f.readers, &mut f.sink, 1, expiry);
    assert_eq!(body(&f.sink.frames[0]), b"late");
    assert!(!f.met());

    // The first EOF read completes after its turn started.
    let new_eof = expiry + Duration::from_secs(1);
    f.clock.set(new_eof);
    park_at_eof(&mut f.readers, &mut f.sink, 1, expiry);
    let new_expiry = new_eof + f.rotation.rotate_wait;
    let early = new_expiry - f.readers.limits.eof_interval;
    assert!(matches!(f.turn_at(early), Reason::Eof));
    assert!(!f.met());
    assert!(matches!(f.turn_at(new_expiry), Reason::Eof));
    assert!(f.met());
}

/// Scenario: An unfinished pass loses policy coverage after two complete misses.
/// Guarantees: The failure vetoes immediately and restoration needs one miss started after the failed pass finished.
#[test]
fn disappearance_unfinished_failure_invalidates_confirmation() {
    let mut f = Fixture::at_eof();
    let expired = f.clock.get() + f.rotation.rotate_wait;
    assert!(matches!(f.turn_at(expired), Reason::Eof));
    f.miss(0, 1);
    f.miss(2, 3);
    assert!(f.met());
    f.begin(4);
    // Failure is receiver-wide, even when the failed name has no locator.
    f.feed(
        5,
        Observation {
            path: "/other".into(),
            locator: None,
            size: None,
            modified: None,
            fingerprint: Vec::new(),
            reason: Some(ScanReason::Policy),
            errno: None,
        },
    );
    assert!(!f.met());
    f.finish(6);
    f.miss(6, 7);
    assert!(!f.met());
    f.miss(8, 9);
    assert!(f.met());
}

/// Scenario: A missing locator returns first with presence alone and then under a validated name of unchanged size.
/// Guarantees: Presence blocks until a fresh miss; validated reappearance requires two misses and preserves EOF inactivity.
#[test]
fn disappearance_presence_and_validated_reappearance() {
    let mut f = Fixture::with_file(b"line\n", Duration::from_millis(250));
    let expiry = f.clock.get() + f.rotation.rotate_wait;
    assert!(matches!(f.turn_at(expiry), Reason::Eof));
    f.miss(0, 1);
    f.miss(2, 3);
    f.begin(4);
    f.observe(5, Some(ScanReason::PresenceOnly));
    assert!(!f.met());
    f.finish(6);
    f.miss(7, 8);
    assert!(f.met());
    f.begin(9);
    f.observe(10, None);
    f.finish(11);
    f.miss(12, 13);
    assert!(!f.met());
    f.miss(14, 15);
    assert!(f.met());
}

/// Scenario: Discovery validates a larger file before two fresh misses, with no intervening reader turn.
/// Guarantees: Known unread bytes invalidate the old EOF interval until read and followed by a new inactivity interval.
#[test]
fn disappearance_discovery_growth_invalidates_eof() {
    let mut f = Fixture::at_eof();
    let expiry = f.clock.get() + f.rotation.rotate_wait;
    assert!(matches!(f.turn_at(expiry), Reason::Eof));
    f.miss(0, 1);
    f.miss(2, 3);
    assert!(f.met());
    f.readers.io.files[0].bytes.extend_from_slice(b"late\n");
    f.begin(4);
    f.observe(5, None);
    f.finish(6);
    f.miss(7, 8);
    f.miss(9, 10);
    assert!(!f.met());

    let read_at = expiry + f.readers.limits.eof_interval;
    f.clock.set(read_at);
    frames(&mut f.readers, &mut f.sink, 1, read_at);
    assert_eq!(body(&f.sink.frames[0]), b"late");
    park_at_eof(&mut f.readers, &mut f.sink, 1, read_at);
    assert!(!f.met());
    assert!(matches!(
        f.turn_at(read_at + f.rotation.rotate_wait),
        Reason::Eof
    ));
    assert!(f.met());
}

/// Scenario: An authorized exclusion arrives while the validated handle remains open.
/// Guarantees: Further complete misses cannot qualify the excluded handle for rotation.
#[test]
fn disappearance_exclusion_supersedes_missing_sequence() {
    let mut f = Fixture::at_eof();
    assert!(matches!(
        f.turn_at(f.clock.get() + f.rotation.rotate_wait),
        Reason::Eof
    ));
    f.miss(0, 1);
    f.miss(2, 3);
    assert!(f.met());
    f.begin(4);
    f.observe(5, Some(ScanReason::Excluded));
    assert!(!f.met());
    f.finish(6);
    f.miss(7, 8);
    f.miss(9, 10);
    assert!(!f.met());
}

/// Scenario: An established include root or binding parent changes identity and later recovers.
/// Guarantees: The old misses cannot confirm disappearance; two complete misses after recovery can.
#[test]
fn disappearance_directory_change_requires_two_fresh_misses() {
    let directory = |inode, reason| Observation {
        path: "/logs".into(),
        locator: Some(FileLocator { device: 1, inode }),
        size: None,
        modified: None,
        fingerprint: Vec::new(),
        reason: Some(reason),
        errno: None,
    };
    for reason in [ScanReason::Directory, ScanReason::RootChanged] {
        let mut f = Fixture::at_eof();
        assert!(matches!(
            f.turn_at(f.clock.get() + f.rotation.rotate_wait),
            Reason::Eof
        ));
        f.begin(0);
        f.feed(1, directory(10, ScanReason::Directory));
        f.finish(2);
        f.miss(3, 4);
        assert!(f.met());
        f.begin(5);
        f.feed(6, directory(11, reason));
        assert!(!f.met());
        f.finish(7);
        f.begin(8);
        f.feed(9, directory(10, ScanReason::Directory));
        f.finish(10);
        assert!(!f.met());
        f.miss(11, 12);
        assert!(f.met());
    }
}

/// Scenario: The next resident EOF probe is scheduled later than rotate_wait.
/// Guarantees: Expiry while idle cannot satisfy inactivity until the scheduled probe succeeds.
#[test]
fn disappearance_waits_for_a_long_reprobe_interval() {
    let interval = Duration::from_secs(10);
    let mut f = Fixture::with_file(b"", interval);
    let first = f.clock.get();
    f.miss(0, 1);
    f.miss(2, 3);
    assert!(matches!(
        f.turn_at(first + f.rotation.rotate_wait),
        Reason::Idle
    ));
    assert!(!f.met());
    assert!(matches!(f.turn_at(first + interval), Reason::Eof));
    assert!(f.met());
}

/// Scenario: An unfinished line needs a content reprobe and its read fails at the deadline.
/// Guarantees: The failure cannot confirm EOF; a successful retry on the same handle can.
#[test]
fn disappearance_waits_for_successful_eof_after_read_error() {
    let mut f = Fixture::with_file(b"partial", Duration::from_millis(250));
    let expiry = f.clock.get() + f.rotation.rotate_wait;
    f.miss(0, 1);
    f.miss(2, 3);
    f.readers.io.read_error = Some(libc::EAGAIN);
    assert!(matches!(f.turn_at(expiry), Reason::Retry(_)));
    assert!(!f.met());
    let retry = f.readers.next_deadline(permission(), &f.sink).unwrap();
    assert!(matches!(f.turn_at(retry), Reason::Eof));
    assert!(f.met());
}

/// Scenario: A descriptor is evicted between reads and reopening it returns ENOENT.
/// Guarantees: A failed reopen preserves partial input and supplies no missing-pass evidence; a later retry can reopen normally.
#[test]
fn closed_handle_reopen_failure_keeps_unfinished_input() {
    let now = Instant::now();
    let mut readers = scheduler(
        &[b"partial", b""],
        32,
        32,
        2,
        1,
        FramingConfig::Line(line()),
    );
    let mut sink = Sink::default();
    let first = tick(&mut readers, &mut sink, now);
    assert_eq!(first.key, Some(ReaderKey(0, 0)));
    let peer = tick(&mut readers, &mut sink, now);
    assert_eq!(peer.key, Some(ReaderKey(1, 0)));
    readers.io.open_error = Some(libc::ENOENT);
    let later = now + readers.limits.eof_interval;
    let failure = tick(&mut readers, &mut sink, later);
    assert_eq!(failure.key, first.key);
    assert!(matches!(failure.reason, Reason::Retry(_)), "{failure:?}");
    let reader = readers.readers[0].as_ref().unwrap();
    assert!(reader.handle.is_none());
    assert!(reader.partial.as_ref().unwrap().unfinished());
    assert!(reader.disappearance.absent_since.is_none());
    readers.io.files[0].bytes.push(b'\n');
    frames(&mut readers, &mut sink, 1, later + Duration::from_secs(1));
    assert_eq!(body(&sink.frames[0]), b"partial");
}

/// Scenario: A native tmpfs file loses its last name and a writer appends after the reader's quiet interval.
/// Guarantees: Without discovery the validated handle captures late bytes, and confirmation requires a fresh quiet interval and same-handle size check.
#[test]
#[ignore = "requires writable Linux tmpfs at /dev/shm; run in filesystem qualification"]
fn resident_unlink_captures_late_write_on_tmpfs() {
    resident_unlink_fixture(tempfile::tempdir_in("/dev/shm").unwrap(), true);
}

/// Scenario: A file loses its last name on a backend outside the resident-unlink qualification.
/// Guarantees: Fresh zero links and repeated EOF checks alone cannot replace discovery on this backend.
#[test]
#[ignore = "requires a non-tmpfs temporary directory; run in filesystem qualification"]
fn resident_unlink_unqualified_backend_requires_discovery() {
    resident_unlink_fixture(tempfile::tempdir().unwrap(), false);
}

fn resident_unlink_fixture(dir: tempfile::TempDir, qualified: bool) {
    use crate::receivers::filelog_receiver::source::{SourceFile, SymlinkPolicy};
    use io::ResidentUnlinkBackend;
    use std::{fs, fs::File, io::Write, os::unix::fs::MetadataExt};

    let path = dir.path().join("app.log");
    let mount = File::open("/dev/shm").unwrap();
    let backend = ResidentUnlinkBackend::tmpfs(&mount).unwrap().unwrap();
    let parent = File::open(dir.path()).unwrap();
    let mut writer = File::create(&path).unwrap();
    let locator = FileLocator::from_metadata(&writer.metadata().unwrap());
    let alias = dir.path().join("alias");
    fs::hard_link(&path, &alias).unwrap();
    let io = io::LinuxIo(move |_: ReaderKey, cancel: &mut dyn FnMut() -> bool| {
        SourceFile::open_at(
            &parent,
            c"app.log",
            SymlinkPolicy::Reject,
            Some(locator),
            cancel,
        )
        .map_err(Into::into)
    });
    let config = FramingConfig::Line(line());
    let limits = Limits {
        readers: 1,
        descriptors: 1,
        read_bytes: 64,
        steps: 64,
        partial_bytes: config.slot_bytes(64).unwrap(),
        eof_interval: Duration::from_millis(250),
    };
    let now = Instant::now();
    let mut readers = Scheduler::new(io, limits, config, Arc::default(), now).unwrap();
    let registry =
        Arc::new(crate::receivers::filelog_receiver::identity::lease::LeaseRegistry::default());
    let owner = registry.owner("unlink-test", 1).unwrap();
    let key = readers
        .admit(test_seed(0, owner.acquire(locator).unwrap()))
        .unwrap();
    let clock = Rc::new(Cell::new(now));
    let eof_clock = Rc::clone(&clock);
    readers.eof_clock = Box::new(move || eof_clock.get());
    let mut sink = Sink::default();
    let rotation = Rotation::default();
    let mut eof = |s: &mut Scheduler<_>, at| {
        clock.set(at);
        for _ in 0..16 {
            let turn = s.turn(at, permission(), &mut sink, Waker::noop(), || false);
            match turn.reason {
                Reason::Eof => return,
                Reason::Progress => {}
                other => panic!("unexpected turn: {other:?}"),
            }
        }
        panic!("did not reach EOF");
    };
    let probe = |s: &mut Scheduler<_>| {
        s.probe_resident_unlink(key, &backend, &rotation, &mut || false)
            .unwrap()
    };
    eof(&mut readers, now);
    fs::remove_file(path).unwrap();
    assert_eq!(probe(&mut readers), ResidentUnlink::Unavailable);
    fs::remove_file(alias).unwrap();
    assert_eq!(writer.metadata().unwrap().nlink(), 0);
    if qualified {
        assert_eq!(probe(&mut readers), ResidentUnlink::Following);
    }
    readers.pin(key).unwrap();
    let deadline = now + rotation.rotate_wait;
    eof(&mut readers, deadline);
    if !qualified {
        assert_eq!(probe(&mut readers), ResidentUnlink::Unavailable);
        return;
    }
    // The write races the cutoff after the scheduled EOF probe.
    writer.write_all(b"late\n").unwrap();
    assert_eq!(probe(&mut readers), ResidentUnlink::Following);
    let read_at = deadline + readers.limits.eof_interval;
    eof(&mut readers, read_at);
    eof(
        &mut readers,
        read_at + rotation.rotate_wait - Duration::from_millis(250),
    );
    assert_eq!(probe(&mut readers), ResidentUnlink::Following);
    eof(&mut readers, read_at + rotation.rotate_wait);
    assert_eq!(probe(&mut readers), ResidentUnlink::ConfirmationEligible);
    assert_eq!(sink.frames.len(), 1);
    assert_eq!(body(&sink.frames[0]), b"late");
}
