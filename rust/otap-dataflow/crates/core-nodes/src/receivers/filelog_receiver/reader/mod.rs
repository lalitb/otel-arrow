// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded logical-reader ownership, fair source-byte scheduling, and
//! descriptor rotation.
//!
//! This synchronous table belongs exclusively to the future dedicated
//! read/checkpoint OS thread. It performs blocking open and read operations,
//! but owns no async runtime, decoder, framer, Arrow builder, or checkpoint
//! mutation. Exactly one source-byte turn may be outstanding, allowing later
//! stages to consume bytes without read-ahead and return the reusable bounded
//! buffer before another file is served.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::collections::{TryReserveError, hash_map::Entry};
#[cfg(test)]
use std::io as test_io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use thiserror::Error;

use super::checkpoint::{
    CommittedFrontierGuard, CommittedFrontierWindow, FileId, FramingResume, LifecycleState, Locator,
};
use super::config::RuntimeConfig;
use super::discovery::DiscoveredCandidate;
use super::environment::{
    DescriptorPressure, DescriptorPressureError, EnvironmentalBackoff, EnvironmentalErrorClass,
    EnvironmentalOperation, classify_io_error,
};
use super::identity::IdentityError;
use super::identity::matcher::{IdentityMatch, ResolvedIdentity};
#[cfg(test)]
use super::identity::platform::collect_consistent_fingerprint_cancellable_with_hook;
use super::identity::platform::{
    ReopenCandidate, collect_consistent_fingerprint_cancellable, encode_advisory_path,
    read_fingerprint_cancellable, read_source_at_cancellable, reopen_candidate_at_cancellable,
};
use super::lease::{LeaseError, ReceiverLeaseScope, RuntimeFileLease, register_receiver_scope};

/// Validated limits and identity parameters consumed by the reader table.
#[derive(Clone, Debug)]
pub(crate) struct ReaderSettings {
    max_readers: usize,
    max_open_files: usize,
    max_read_bytes_per_turn: usize,
    eof_probe_interval: Duration,
    fingerprint_bytes: u16,
    ignored_header_bytes: u32,
    follow_symlinks: bool,
}

impl ReaderSettings {
    /// Extracts reader settings from a fully validated receiver
    /// configuration.
    pub(crate) fn from_runtime(config: &RuntimeConfig) -> Self {
        Self {
            max_readers: config.limits.max_tracked_files as usize,
            max_open_files: config.limits.max_open_files as usize,
            max_read_bytes_per_turn: usize::try_from(config.limits.max_read_bytes_per_turn)
                .expect("validated max_read_bytes_per_turn fits usize"),
            eof_probe_interval: config.reader.eof_reprobe_interval,
            fingerprint_bytes: u16::try_from(config.identity.fingerprint_bytes)
                .expect("validated fingerprint_bytes fits u16"),
            ignored_header_bytes: u32::try_from(config.identity.ignored_header_bytes)
                .expect("validated ignored_header_bytes fits u32"),
            follow_symlinks: config.follow_symlinks,
        }
    }
}

/// Fail-closed reader-table and source-I/O errors.
#[derive(Debug, Error)]
pub(crate) enum ReaderError {
    /// An independently constructed setting violates validated config
    /// invariants.
    #[error("invalid filelog reader setting: {reason}")]
    InvalidSettings {
        /// Exact rejected invariant.
        reason: &'static str,
    },
    /// Preallocating one bounded reader collection or source buffer failed.
    #[error("could not allocate bounded filelog reader resource '{resource}'")]
    AllocationFailed {
        /// Resource whose configured bound could not be reserved.
        resource: &'static str,
        /// Allocation failure from the collection.
        #[source]
        source: TryReserveError,
    },
    /// Process-local runtime ownership failed.
    #[error(transparent)]
    Lease(#[from] LeaseError),
    /// Shared receiver descriptor-pressure state failed.
    #[error(transparent)]
    DescriptorPressure(#[from] DescriptorPressureError),
    /// Candidate path or identity evidence could not satisfy the bounded
    /// platform contract.
    #[error(transparent)]
    Identity(#[from] IdentityError),
    /// A resolved identity is not eligible for a live reader.
    #[error("filelog identity {file_id:?} is in lifecycle state {state:?}, not active")]
    InactiveIdentity {
        /// Durable identity that cannot be scheduled.
        file_id: FileId,
        /// Current durable lifecycle state.
        state: LifecycleState,
    },
    /// A candidate would exceed the configured logical-reader population.
    #[error("filelog reader capacity is exhausted at {max} logical readers")]
    ReaderCapacityExhausted {
        /// Configured logical-reader maximum.
        max: usize,
    },
    /// A durable identity is already present in the table.
    #[error("filelog reader table already contains file {file_id:?}")]
    DuplicateFileId {
        /// Repeated durable identity.
        file_id: FileId,
    },
    /// A runtime locator is already represented by another logical reader.
    #[error(
        "filelog reader locator {locator:?} already belongs to {existing_file_id:?}, not {file_id:?}"
    )]
    DuplicateLocator {
        /// Repeated live locator.
        locator: Locator,
        /// Existing logical owner.
        existing_file_id: FileId,
        /// Proposed logical owner.
        file_id: FileId,
    },
    /// Candidate evidence and resolved durable state disagree.
    #[error("invalid filelog reader admission for {file_id:?}: {reason}")]
    InvalidAdmission {
        /// Durable identity being admitted.
        file_id: FileId,
        /// Exact violated invariant.
        reason: &'static str,
    },
    /// A requested file is not represented by the table.
    #[error("filelog reader table does not contain file {file_id:?}")]
    UnknownFile {
        /// Missing durable identity.
        file_id: FileId,
    },
    /// A locator is not represented by the table.
    #[error("filelog reader table does not contain locator {locator:?}")]
    UnknownLocator {
        /// Missing runtime locator.
        locator: Locator,
    },
    /// A reader operation is invalid in its current scheduling state.
    #[error("filelog reader {file_id:?} cannot {operation} while {state}")]
    InvalidState {
        /// Durable identity in the wrong state.
        file_id: FileId,
        /// Requested operation.
        operation: &'static str,
        /// Stable state description.
        state: &'static str,
    },
    /// A returned read turn does not match the table's outstanding turn.
    #[error("stale or mismatched filelog read turn {ticket} for {file_id:?}")]
    InvalidTurn {
        /// Durable identity named by the turn.
        file_id: FileId,
        /// Monotonic turn ticket.
        ticket: u64,
    },
    /// A caller claimed more bytes than one turn returned.
    #[error(
        "filelog read turn for {file_id:?} consumed {consumed} bytes from a {available}-byte buffer"
    )]
    InvalidTurnConsumption {
        /// Durable identity named by the turn.
        file_id: FileId,
        /// Claimed source bytes.
        consumed: usize,
        /// Bytes returned by the source read.
        available: usize,
    },
    /// A progress observation violates epoch or monotonic-frontier rules.
    #[error("invalid committed progress for filelog reader {file_id:?}: {reason}")]
    InvalidProgress {
        /// Durable identity whose progress was rejected.
        file_id: FileId,
        /// Exact violated invariant.
        reason: &'static str,
    },
    /// Source-byte offset arithmetic overflowed.
    #[error("filelog reader source offset overflowed for {file_id:?}")]
    OffsetOverflow {
        /// Durable identity whose offset could not advance.
        file_id: FileId,
    },
    /// A monotonic reader counter exhausted `u64`.
    #[error("filelog reader counter '{counter}' overflowed")]
    CounterOverflow {
        /// Counter that could not advance.
        counter: &'static str,
    },
    /// The configured EOF probe interval cannot be represented from the
    /// supplied clock value.
    #[error("filelog reader EOF probe deadline overflowed")]
    DeadlineOverflow,
    /// Opening a logical reader failed identity revalidation.
    #[error("could not revalidate filelog reader {file_id:?}: {source}")]
    Reopen {
        /// Durable identity being reopened.
        file_id: FileId,
        /// Structured identity failure.
        #[source]
        source: IdentityError,
    },
    /// One bounded positioned source read failed.
    #[error("could not read filelog source {path} for {file_id:?}: {source}")]
    Read {
        /// Durable identity being served.
        file_id: FileId,
        /// Advisory path for diagnostics only.
        path: PathBuf,
        /// Operating-system read failure.
        #[source]
        source: std::io::Error,
    },
    /// Internal bounded indexes no longer agree.
    #[error("filelog reader table integrity check failed: {reason}")]
    Inconsistent {
        /// Exact failed internal invariant.
        reason: &'static str,
    },
}

/// What the caller wants the scheduler to do after consuming a source turn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TurnDisposition {
    /// Put the reader at the round-robin tail for another bounded turn.
    Ready,
    /// Keep the reader paused until an explicit [`ReaderTable::make_ready`].
    Paused,
}

/// A single bounded source-byte turn.
///
/// The caller may consume any prefix, then must return this value through
/// [`ReaderTable::complete_turn`]. Unconsumed bytes are deterministically
/// reread from their original source offset.
#[derive(Debug)]
pub(crate) struct ReadTurn {
    ticket: u64,
    file_id: FileId,
    file_epoch: u32,
    committed_offset: u64,
    framing_resume: FramingResume,
    source_offset: u64,
    bytes: Vec<u8>,
}

impl ReadTurn {
    /// Durable identity served by this turn.
    #[must_use]
    pub(crate) const fn file_id(&self) -> FileId {
        self.file_id
    }

    /// Durable epoch observed when the turn was issued.
    #[must_use]
    pub(crate) const fn file_epoch(&self) -> u32 {
        self.file_epoch
    }

    /// Ack-gated source-byte frontier from which discarded framing state
    /// must be reconstructed.
    #[must_use]
    pub(crate) const fn committed_offset(&self) -> u64 {
        self.committed_offset
    }

    /// Durable framing state paired atomically with the committed offset.
    #[must_use]
    pub(crate) const fn framing_resume(&self) -> FramingResume {
        self.framing_resume
    }

    /// Source-byte offset of the first returned byte.
    #[must_use]
    pub(crate) const fn source_offset(&self) -> u64 {
        self.source_offset
    }

    /// Source bytes returned by the bounded positioned read.
    #[must_use]
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Explicit handoff required before a resident descriptor is evicted.
///
/// The caller must discard every uncommitted decoder/framer object for the
/// victim before confirming. The runtime lease and durable frontier remain
/// owned by the logical reader.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct EvictionRequest {
    ticket: u64,
    /// Closed target waiting for an open-file slot.
    pub(crate) target_file_id: FileId,
    /// Least-recently-served resident selected as the victim.
    pub(crate) victim_file_id: FileId,
    /// Durable source-byte frontier to which the victim will rewind.
    pub(crate) committed_offset: u64,
    /// Current uncommitted source-byte frontier.
    pub(crate) read_offset: u64,
}

/// Why a removed logical reader cannot continue through its old handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RemovalDisposition {
    /// The open handle is retained for late writes.
    HandleRetained,
    /// The descriptor had already rotated out; a path cannot recover an
    /// unlinked native identity, so later rotation policy must decide.
    DescriptorAbsent,
}

/// Result of refreshing queued discovery evidence from a live reader.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CandidateEvidenceRefresh {
    /// Lifecycle cancellation stopped evidence sampling.
    Cancelled,
    /// The queued size and fingerprint now match a stable handle observation.
    Refreshed,
    /// No descriptor is resident, so the queued observation remains the only
    /// available evidence and the reader frontier cannot advance meanwhile.
    DescriptorAbsent,
    /// The handle changed during bounded evidence collection and must be
    /// retried after a bounded delay.
    Retry,
}

/// Result of one scheduler poll.
#[derive(Debug)]
pub(crate) enum ReaderPoll {
    /// Lifecycle cancellation stopped the poll before another source
    /// operation could begin.
    Cancelled,
    /// One bounded source-byte turn is ready for decoding/framing.
    Data(ReadTurn),
    /// One reader observed temporary EOF and has a bounded next probe.
    EndOfFile {
        /// Durable identity at EOF.
        file_id: FileId,
        /// Current durable epoch.
        file_epoch: u32,
        /// Uncommitted source-byte frontier at EOF.
        source_offset: u64,
        /// Earliest automatic re-probe.
        next_probe: Instant,
    },
    /// The open handle is observably shorter than source bytes already
    /// consumed or committed.
    Truncated {
        /// Durable identity whose handle shrank.
        file_id: FileId,
        /// Current durable epoch.
        file_epoch: u32,
        /// Ack-gated source-byte frontier.
        committed_offset: u64,
        /// Current in-memory source-byte frontier.
        read_offset: u64,
        /// Size observed from the same open handle.
        observed_size: u64,
        /// Fresh bounded fingerprint evidence from the same open handle.
        observed_fingerprint: Vec<u8>,
    },
    /// The file changed while EOF fingerprint evidence was sampled.
    EvidenceUnstable {
        /// Durable identity whose observation must be retried.
        #[allow(
            dead_code,
            reason = "diagnostic and test callers correlate the poll outcome while production aggregates it"
        )]
        file_id: FileId,
        /// Earliest automatic retry.
        next_probe: Instant,
    },
    /// A source operation failed environmentally and was scheduled for
    /// bounded retry without changing durable or lifecycle state.
    EnvironmentalBackoff {
        /// Durable identity retaining its reader state.
        #[allow(
            dead_code,
            reason = "diagnostic and test callers correlate the poll outcome while production aggregates it"
        )]
        file_id: FileId,
        /// Operation that could not proceed.
        operation: EnvironmentalOperation,
        /// Fixed error class.
        error: EnvironmentalErrorClass,
        /// Checked retry deadline.
        next_probe: Instant,
    },
    /// A descriptor can rotate only after caller-owned uncommitted state is
    /// discarded.
    EvictionRequired(EvictionRequest),
    /// Every descriptor is temporarily non-evictable, for example because
    /// removed handles are pinned for late writes.
    DescriptorCapacityBlocked {
        /// Closed reader waiting for a slot.
        #[allow(
            dead_code,
            reason = "diagnostic and test callers correlate the poll outcome while production aggregates it"
        )]
        file_id: FileId,
    },
    /// A removed reader no longer has the native handle needed for reliable
    /// late-write capture.
    RemovedWithoutDescriptor {
        /// Affected durable identity.
        file_id: FileId,
    },
    /// No source is currently ready.
    Idle {
        /// Earliest automatic EOF re-probe, if any.
        next_probe: Option<Instant>,
    },
}

/// Bounded reader populations and checked monotonic activity counters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReaderStats {
    /// Logical readers retaining runtime leases.
    pub(crate) tracked_readers: usize,
    /// Resident source descriptors.
    pub(crate) open_files: usize,
    /// Readers in the round-robin ready queue.
    pub(crate) ready_readers: usize,
    /// Readers waiting for EOF re-probe.
    pub(crate) eof_readers: usize,
    /// Readers waiting for bounded source-environment retry.
    pub(crate) environmental_backoff_readers: usize,
    /// Readers removed from discovery but not finalized.
    pub(crate) removed_readers: usize,
    /// Readers waiting for a resident descriptor slot.
    pub(crate) descriptor_blocked_readers: usize,
    /// Bounded positioned-read attempts.
    pub(crate) read_turns: u64,
    /// Source bytes returned by operating-system reads, including replay.
    pub(crate) source_bytes_read: u64,
    /// First descriptor opens.
    pub(crate) opens: u64,
    /// Descriptor reopens after temporary closure.
    pub(crate) reopens: u64,
    /// Confirmed least-recently-served evictions.
    pub(crate) descriptor_evictions: u64,
    /// Failed descriptor reopens after an earlier successful open.
    pub(crate) descriptor_reopen_failures: u64,
    /// Uncommitted source bytes rewound by confirmed eviction.
    pub(crate) source_bytes_rewound: u64,
    /// Temporary EOF observations.
    pub(crate) eof_observations: u64,
    /// Ordinary EOF deadlines promoted for another probe.
    pub(crate) eof_reprobes: u64,
    /// Removed readers retaining late-write-capable descriptors.
    pub(crate) pinned_rotated_handles: usize,
    /// Age of the oldest pinned rotated descriptor.
    pub(crate) pinned_rotated_oldest_age_ns: u64,
}

/// Fixed-size runtime-lease observations transferred after admission.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct LeaseObservations {
    pub(crate) wait_ns: u64,
    pub(crate) attempts: u64,
    pub(crate) failures: u64,
    pub(crate) contentions: u64,
}

/// Allocation-free snapshot of one logical reader's source frontiers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ReaderFrontier {
    /// Durable identity represented by the logical reader.
    pub(crate) file_id: FileId,
    /// Current durable stream epoch.
    pub(crate) file_epoch: u32,
    /// Ack-gated source-byte frontier.
    pub(crate) committed_offset: u64,
    /// Current provisional source-byte frontier.
    pub(crate) read_offset: u64,
    /// Durable framing state paired with `committed_offset`.
    pub(crate) framing_resume: FramingResume,
    /// Whether discovery still observes the locator.
    pub(crate) present: bool,
    /// Whether the late-write-capable native descriptor is resident.
    pub(crate) descriptor_resident: bool,
    /// Whether batch sealing paused this reader for an exact later resume.
    pub(crate) paused_for_batch: bool,
}

/// Reader-owned path context used to project one framed record.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReaderRecordContext<'a> {
    /// Operator-facing path that matched discovery.
    pub(crate) matched_path: &'a Path,
    /// Canonical target path opened by discovery.
    #[allow(
        dead_code,
        reason = "retained for identity diagnostics while OTAP projection intentionally uses the matched path"
    )]
    pub(crate) resolved_path: &'a Path,
}

/// Reader-owned evidence needed to preflight one discovery update before it
/// can mutate durable identity state.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReaderIdentityContext<'a> {
    /// Durable identity currently associated with the runtime locator.
    pub(crate) file_id: FileId,
    /// Current durable stream epoch.
    pub(crate) file_epoch: u32,
    /// Ack-gated source offset that updated evidence must still contain.
    pub(crate) committed_offset: u64,
    /// Current provisional source offset that updated evidence must still
    /// contain.
    pub(crate) read_offset: u64,
    /// Durable fingerprint prefix that updated evidence must extend.
    pub(crate) durable_fingerprint: &'a [u8],
}

#[derive(Debug)]
struct ActivityCounters {
    read_turns: u64,
    source_bytes_read: u64,
    opens: u64,
    reopens: u64,
    descriptor_evictions: u64,
    descriptor_reopen_failures: u64,
    source_bytes_rewound: u64,
    eof_observations: u64,
    eof_reprobes: u64,
}

impl ActivityCounters {
    const fn new() -> Self {
        Self {
            read_turns: 0,
            source_bytes_read: 0,
            opens: 0,
            reopens: 0,
            descriptor_evictions: 0,
            descriptor_reopen_failures: 0,
            source_bytes_rewound: 0,
            eof_observations: 0,
            eof_reprobes: 0,
        }
    }
}

#[derive(Debug)]
struct ResidentReader {
    file: std::fs::File,
    last_served_sequence: u64,
}

#[derive(Debug)]
enum ScheduleState {
    Ready,
    Eof {
        next_probe: Instant,
    },
    DescriptorBlocked,
    Paused,
    InFlight {
        ticket: u64,
        source_offset: u64,
        available: usize,
    },
}

impl ScheduleState {
    const fn name(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Eof { .. } => "EOF",
            Self::DescriptorBlocked => "blocked on descriptor capacity",
            Self::Paused => "paused",
            Self::InFlight { .. } => "a read turn is outstanding",
        }
    }
}

#[derive(Debug)]
struct LogicalReader {
    file_id: FileId,
    file_epoch: u32,
    locator: Locator,
    lease: RuntimeFileLease,
    matched_path: PathBuf,
    resolved_path: PathBuf,
    durable_fingerprint: Vec<u8>,
    committed_offset: u64,
    read_offset: u64,
    framing_resume: FramingResume,
    /// The durable committed-frontier guard paired with `committed_offset`.
    /// Never mutated except atomically alongside `committed_offset`.
    committed_frontier_guard: CommittedFrontierGuard,
    /// The exact real raw bytes backing `committed_frontier_guard`, owned by
    /// this reader so a framer can be (re)constructed after Ack, Nack
    /// rewind, batch seal, descriptor eviction, drain rewind, or carry-over
    /// without ever rereading the source. `None` only until the descriptor
    /// has been opened at least once and the window validated against
    /// `committed_frontier_guard`.
    committed_frontier_window: Option<CommittedFrontierWindow>,
    present: bool,
    ever_opened: bool,
    resident: Option<ResidentReader>,
    pinned_since: Option<Instant>,
    schedule: ScheduleState,
    paused_for_batch: bool,
}

/// Single-thread-owned logical-reader table and fair scheduler.
#[derive(Debug)]
pub(crate) struct ReaderTable {
    settings: ReaderSettings,
    lease_scope: ReceiverLeaseScope,
    readers: HashMap<FileId, LogicalReader>,
    by_locator: HashMap<Locator, FileId>,
    ready: VecDeque<FileId>,
    eof_deadlines: BTreeSet<(Instant, FileId)>,
    environmental_waiting: HashSet<FileId>,
    environmental_failures: HashMap<FileId, EnvironmentalBackoff>,
    descriptor_pressure: Arc<DescriptorPressure>,
    descriptor_pressure_waiting: HashMap<FileId, EnvironmentalOperation>,
    descriptor_blocked: BTreeSet<FileId>,
    pinned_rotated: BTreeSet<(Instant, FileId)>,
    open_count: usize,
    removed_count: usize,
    service_sequence: u64,
    turn_sequence: u64,
    eviction_sequence: u64,
    pending_eviction: Option<EvictionRequest>,
    deferred_eviction_target: Option<FileId>,
    batch_pause_order_prepared: bool,
    read_buffer: Option<Vec<u8>>,
    counters: ActivityCounters,
    lease_observations: LeaseObservations,
    shutdown_requested: Arc<AtomicBool>,
    #[cfg(test)]
    fail_next_revoked_release: bool,
    #[cfg(test)]
    next_source_read_gate: Option<ReaderPollGate>,
    #[cfg(test)]
    fail_next_open: Option<test_io::Error>,
    #[cfg(test)]
    fail_next_read: Option<test_io::Error>,
    #[cfg(test)]
    next_evidence_refresh_gate: Mutex<Option<ReaderPollGate>>,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct ReaderPollGateState {
    entered: bool,
    released: bool,
}

#[cfg(test)]
#[derive(Clone, Debug, Default)]
pub(crate) struct ReaderPollGate {
    state: Arc<(Mutex<ReaderPollGateState>, Condvar)>,
}

#[cfg(test)]
impl ReaderPollGate {
    fn block(&self) {
        let (state, condition) = &*self.state;
        let mut state = state.lock().expect("reader poll gate lock poisoned");
        state.entered = true;
        condition.notify_all();
        while !state.released {
            state = condition
                .wait(state)
                .expect("reader poll gate lock poisoned while blocked");
        }
    }

    pub(crate) fn wait_until_entered(&self, timeout: Duration) -> bool {
        let (state, condition) = &*self.state;
        let state = state.lock().expect("reader poll gate lock poisoned");
        let (state, _) = condition
            .wait_timeout_while(state, timeout, |state| !state.entered)
            .expect("reader poll gate lock poisoned while waiting");
        state.entered
    }

    pub(crate) fn release(&self) {
        let (state, condition) = &*self.state;
        let mut state = state.lock().expect("reader poll gate lock poisoned");
        state.released = true;
        condition.notify_all();
    }
}

#[derive(Clone, Copy)]
struct ReadLimit {
    file_id: FileId,
    end_offset: u64,
}

#[derive(Debug, Eq, PartialEq)]
enum FingerprintObservation {
    Stable { fingerprint: Vec<u8>, size: u64 },
    Retry,
}

enum OpenReaderOutcome {
    Compatible,
    Truncated {
        observed_size: u64,
        observed_fingerprint: Vec<u8>,
    },
}

fn classify_fingerprint_observation(
    observation: Result<(Vec<u8>, u64), IdentityError>,
) -> Result<FingerprintObservation, ReaderError> {
    match observation {
        Ok((fingerprint, size)) => Ok(FingerprintObservation::Stable { fingerprint, size }),
        Err(IdentityError::CandidateChangedDuringIdentity { .. }) => {
            Ok(FingerprintObservation::Retry)
        }
        Err(error) => Err(ReaderError::Identity(error)),
    }
}

fn classify_cancellable_fingerprint_observation(
    observation: Result<Option<(Vec<u8>, u64)>, IdentityError>,
) -> Result<Option<FingerprintObservation>, ReaderError> {
    match observation {
        Ok(Some(observation)) => classify_fingerprint_observation(Ok(observation)).map(Some),
        Ok(None) => Ok(None),
        Err(error) => classify_fingerprint_observation(Err(error)).map(Some),
    }
}

fn environmental_reader_error(
    error: &ReaderError,
) -> Option<(EnvironmentalOperation, EnvironmentalErrorClass)> {
    match error {
        ReaderError::Reopen {
            source: IdentityError::Io { source, .. },
            ..
        } => Some((EnvironmentalOperation::Open, classify_io_error(source))),
        ReaderError::Read { source, .. } => {
            Some((EnvironmentalOperation::Read, classify_io_error(source)))
        }
        ReaderError::Identity(IdentityError::Io { source, .. }) => {
            Some((EnvironmentalOperation::Inspect, classify_io_error(source)))
        }
        _ => None,
    }
}

impl ReaderTable {
    /// Creates a bounded table and reserves its one shared source-read
    /// buffer before any file can be admitted.
    #[cfg(test)]
    pub(crate) fn new(settings: ReaderSettings) -> Result<Self, ReaderError> {
        Self::new_with_shutdown_signal_and_pressure(
            settings,
            Arc::new(AtomicBool::new(false)),
            Arc::new(DescriptorPressure::default()),
        )
    }

    /// Creates a bounded table with lifecycle cancellation and one
    /// receiver-global descriptor-pressure state shared with discovery.
    pub(crate) fn new_with_shutdown_signal_and_pressure(
        settings: ReaderSettings,
        shutdown_requested: Arc<AtomicBool>,
        descriptor_pressure: Arc<DescriptorPressure>,
    ) -> Result<Self, ReaderError> {
        if settings.max_readers == 0 {
            return Err(ReaderError::InvalidSettings {
                reason: "max_readers must be greater than zero",
            });
        }
        if settings.max_open_files == 0 || settings.max_open_files > settings.max_readers {
            return Err(ReaderError::InvalidSettings {
                reason: "max_open_files must be in 1..=max_readers",
            });
        }
        if settings.max_read_bytes_per_turn == 0 {
            return Err(ReaderError::InvalidSettings {
                reason: "max_read_bytes_per_turn must be greater than zero",
            });
        }
        if settings.eof_probe_interval.is_zero() {
            return Err(ReaderError::InvalidSettings {
                reason: "eof_probe_interval must be greater than zero",
            });
        }

        let mut readers = HashMap::new();
        readers
            .try_reserve(settings.max_readers)
            .map_err(|source| ReaderError::AllocationFailed {
                resource: "logical readers",
                source,
            })?;
        let mut by_locator = HashMap::new();
        by_locator
            .try_reserve(settings.max_readers)
            .map_err(|source| ReaderError::AllocationFailed {
                resource: "locator index",
                source,
            })?;
        let mut ready = VecDeque::new();
        ready.try_reserve(settings.max_readers).map_err(|source| {
            ReaderError::AllocationFailed {
                resource: "ready queue",
                source,
            }
        })?;
        let mut environmental_waiting = HashSet::new();
        environmental_waiting
            .try_reserve(settings.max_readers)
            .map_err(|source| ReaderError::AllocationFailed {
                resource: "environmental retry population",
                source,
            })?;
        let mut environmental_failures = HashMap::new();
        environmental_failures
            .try_reserve(settings.max_readers)
            .map_err(|source| ReaderError::AllocationFailed {
                resource: "environmental retry state",
                source,
            })?;
        let mut descriptor_pressure_waiting = HashMap::new();
        descriptor_pressure_waiting
            .try_reserve(settings.max_readers)
            .map_err(|source| ReaderError::AllocationFailed {
                resource: "descriptor-pressure wait population",
                source,
            })?;
        let mut read_buffer = Vec::new();
        read_buffer
            .try_reserve_exact(settings.max_read_bytes_per_turn)
            .map_err(|source| ReaderError::AllocationFailed {
                resource: "source read buffer",
                source,
            })?;
        read_buffer.resize(settings.max_read_bytes_per_turn, 0);

        let max_leases =
            u32::try_from(settings.max_readers).map_err(|_| ReaderError::InvalidSettings {
                reason: "max_readers must fit the runtime lease registry",
            })?;
        let lease_scope = register_receiver_scope(max_leases)?;
        Ok(Self {
            settings,
            lease_scope,
            readers,
            by_locator,
            ready,
            eof_deadlines: BTreeSet::new(),
            environmental_waiting,
            environmental_failures,
            descriptor_pressure,
            descriptor_pressure_waiting,
            descriptor_blocked: BTreeSet::new(),
            pinned_rotated: BTreeSet::new(),
            open_count: 0,
            removed_count: 0,
            service_sequence: 0,
            turn_sequence: 0,
            eviction_sequence: 0,
            pending_eviction: None,
            deferred_eviction_target: None,
            batch_pause_order_prepared: false,
            read_buffer: Some(read_buffer),
            counters: ActivityCounters::new(),
            lease_observations: LeaseObservations::default(),
            shutdown_requested,
            #[cfg(test)]
            fail_next_revoked_release: false,
            #[cfg(test)]
            next_source_read_gate: None,
            #[cfg(test)]
            fail_next_open: None,
            #[cfg(test)]
            fail_next_read: None,
            #[cfg(test)]
            next_evidence_refresh_gate: Mutex::new(None),
        })
    }

    fn cancellation_requested(&self) -> bool {
        self.shutdown_requested.load(Ordering::Acquire)
    }

    /// Adds one durably resolved active identity and acquires its runtime
    /// lease before the reader can be scheduled.
    pub(crate) fn insert(
        &mut self,
        candidate: DiscoveredCandidate,
        resolved: ResolvedIdentity,
    ) -> Result<(), ReaderError> {
        if resolved.lifecycle_state != LifecycleState::Active {
            return Err(ReaderError::InactiveIdentity {
                file_id: resolved.file_id,
                state: resolved.lifecycle_state,
            });
        }
        if self.readers.len() >= self.settings.max_readers {
            return Err(ReaderError::ReaderCapacityExhausted {
                max: self.settings.max_readers,
            });
        }
        if self.readers.contains_key(&resolved.file_id) {
            return Err(ReaderError::DuplicateFileId {
                file_id: resolved.file_id,
            });
        }
        if let Some(existing_file_id) = self.by_locator.get(&candidate.evidence.locator) {
            return Err(ReaderError::DuplicateLocator {
                locator: candidate.evidence.locator,
                existing_file_id: *existing_file_id,
                file_id: resolved.file_id,
            });
        }
        if candidate.evidence.size < resolved.committed_offset {
            return Err(ReaderError::InvalidAdmission {
                file_id: resolved.file_id,
                reason: "candidate size is below the durable committed offset",
            });
        }
        let _bounded_resolved_path = encode_advisory_path(&candidate.resolved_path)?;
        let lease_started = Instant::now();
        let lease_result = self.lease_scope.try_acquire(candidate.evidence.locator);
        self.lease_observations.attempts = self.lease_observations.attempts.saturating_add(1);
        self.lease_observations.wait_ns = self
            .lease_observations
            .wait_ns
            .saturating_add(u64::try_from(lease_started.elapsed().as_nanos()).unwrap_or(u64::MAX));
        let lease = match lease_result {
            Ok(lease) => lease,
            Err(error) => {
                self.lease_observations.failures =
                    self.lease_observations.failures.saturating_add(1);
                if matches!(error, LeaseError::Contended { .. }) {
                    self.lease_observations.contentions =
                        self.lease_observations.contentions.saturating_add(1);
                }
                return Err(ReaderError::Lease(error));
            }
        };
        let file_id = resolved.file_id;
        let locator = candidate.evidence.locator;
        // A genuinely new identity's committed offset is always either `0`
        // (`start_at: beginning`) or exactly the candidate's current size
        // (`start_at: end`, or a recovery-mismatch skip-to-end): in both
        // cases the candidate's own evidence, already read from this same
        // validated handle, is real committed-frontier evidence and never
        // needs to be reread once a descriptor is opened. An existing
        // durable identity resumed mid-file (`ExactLocator`) has no such
        // evidence at `committed_offset`; its real window is read and
        // validated once this reader's own descriptor is (re)opened, in
        // `open_reader`.
        let committed_frontier_window = if resolved.committed_offset == 0 {
            Some(CommittedFrontierWindow::empty())
        } else if matches!(
            resolved.matched_by,
            IdentityMatch::NewDiscovery | IdentityMatch::RecoveryMismatch
        ) && resolved.committed_offset
            == candidate.evidence.committed_frontier_window.end_offset()
        {
            Some(candidate.evidence.committed_frontier_window.clone())
        } else {
            None
        };
        // Whenever a window is already trusted, its own guard is the only
        // consistent evidence for later reopen validation: it is derived
        // from the exact same bytes, never a separately supplied value that
        // could disagree with what was actually adopted. Only the deferred
        // (not yet trusted) case relies on the caller-supplied durable
        // guard, since no window exists yet to derive one from.
        let committed_frontier_guard = match &committed_frontier_window {
            Some(window) => window.guard().map_err(|_| ReaderError::Inconsistent {
                reason: "trusted committed-frontier window failed to produce a guard",
            })?,
            None => resolved.committed_frontier_guard,
        };
        let reader = LogicalReader {
            file_id,
            file_epoch: resolved.file_epoch,
            locator,
            lease,
            matched_path: candidate.matched_path,
            resolved_path: candidate.resolved_path,
            durable_fingerprint: candidate.evidence.fingerprint,
            committed_offset: resolved.committed_offset,
            read_offset: resolved.committed_offset,
            framing_resume: resolved.framing_resume,
            committed_frontier_guard,
            committed_frontier_window,
            present: true,
            ever_opened: false,
            resident: None,
            pinned_since: None,
            schedule: ScheduleState::Ready,
            paused_for_batch: false,
        };
        match self.readers.entry(file_id) {
            Entry::Vacant(slot) => {
                let _ = slot.insert(reader);
            }
            Entry::Occupied(_) => {
                return Err(ReaderError::Inconsistent {
                    reason: "file ID appeared after duplicate admission check",
                });
            }
        }
        if self.by_locator.insert(locator, file_id).is_some() {
            return Err(ReaderError::Inconsistent {
                reason: "locator appeared after duplicate admission check",
            });
        }
        self.ready.push_back(file_id);
        Ok(())
    }

    /// Applies a durably persisted same-locator evidence/path refresh.
    pub(crate) fn update(
        &mut self,
        candidate: DiscoveredCandidate,
        resolved: &ResolvedIdentity,
    ) -> Result<(), ReaderError> {
        let file_id = *self.by_locator.get(&candidate.evidence.locator).ok_or(
            ReaderError::UnknownLocator {
                locator: candidate.evidence.locator,
            },
        )?;
        let _bounded_resolved_path = encode_advisory_path(&candidate.resolved_path)?;
        let should_wake = {
            let reader = self
                .readers
                .get_mut(&file_id)
                .ok_or(ReaderError::Inconsistent {
                    reason: "locator index points to a missing reader",
                })?;
            if resolved.file_id != file_id
                || resolved.file_epoch != reader.file_epoch
                || resolved.committed_offset != reader.committed_offset
                || resolved.lifecycle_state != LifecycleState::Active
            {
                return Err(ReaderError::InvalidAdmission {
                    file_id,
                    reason: "persisted update does not match the live reader frontier",
                });
            }
            if !reader.present {
                return Err(ReaderError::InvalidAdmission {
                    file_id,
                    reason: "a removed locator cannot be revived before finalization",
                });
            }
            if !candidate
                .evidence
                .fingerprint
                .starts_with(&reader.durable_fingerprint)
            {
                return Err(ReaderError::InvalidAdmission {
                    file_id,
                    reason: "updated fingerprint does not extend durable evidence",
                });
            }
            if candidate.evidence.size < reader.committed_offset {
                return Err(ReaderError::InvalidAdmission {
                    file_id,
                    reason: "updated candidate size is below committed progress",
                });
            }
            let should_wake = matches!(
                reader.schedule,
                ScheduleState::Eof { .. } | ScheduleState::DescriptorBlocked
            );
            reader.matched_path = candidate.matched_path;
            reader.resolved_path = candidate.resolved_path;
            reader.durable_fingerprint = candidate.evidence.fingerprint;
            should_wake
        };
        self.clear_environmental_success(file_id, None)?;
        if should_wake {
            self.make_ready(file_id)
        } else {
            Ok(())
        }
    }

    /// Marks a locator absent without releasing its logical-reader lease.
    pub(crate) fn mark_removed(
        &mut self,
        locator: Locator,
    ) -> Result<RemovalDisposition, ReaderError> {
        self.mark_removed_at(locator, Instant::now())
    }

    fn mark_removed_at(
        &mut self,
        locator: Locator,
        now: Instant,
    ) -> Result<RemovalDisposition, ReaderError> {
        let file_id = *self
            .by_locator
            .get(&locator)
            .ok_or(ReaderError::UnknownLocator { locator })?;
        self.cancel_eviction_involving(file_id)?;
        let pinned_since = {
            let reader = self
                .readers
                .get_mut(&file_id)
                .ok_or(ReaderError::Inconsistent {
                    reason: "locator index points to a missing reader",
                })?;
            if !reader.present {
                return Err(ReaderError::InvalidState {
                    file_id,
                    operation: "mark removed",
                    state: "already removed",
                });
            }
            reader.present = false;
            let pinned_since = reader.resident.is_some().then_some(now);
            reader.pinned_since = pinned_since;
            pinned_since
        };
        self.removed_count =
            self.removed_count
                .checked_add(1)
                .ok_or(ReaderError::CounterOverflow {
                    counter: "removed readers",
                })?;
        if let Some(pinned_since) = pinned_since {
            if !self.pinned_rotated.insert((pinned_since, file_id)) {
                return Err(ReaderError::Inconsistent {
                    reason: "removed reader pin was already indexed",
                });
            }
            Ok(RemovalDisposition::HandleRetained)
        } else {
            self.pause(file_id)?;
            Ok(RemovalDisposition::DescriptorAbsent)
        }
    }

    /// Promotes due EOF readers, then serves at most one ready reader.
    pub(crate) fn poll(&mut self, now: Instant) -> Result<ReaderPoll, ReaderError> {
        self.poll_inner(now, None)
    }

    /// Serves only `file_id` and never reads beyond the captured drain
    /// frontier `end_offset`.
    pub(crate) fn poll_until(
        &mut self,
        now: Instant,
        file_id: FileId,
        end_offset: u64,
    ) -> Result<ReaderPoll, ReaderError> {
        if let Some(operation) = self.descriptor_pressure_waiting.get(&file_id).copied()
            && let Some(next_probe) = self.descriptor_pressure.retry_at(now)?
        {
            return Ok(ReaderPoll::EnvironmentalBackoff {
                file_id,
                operation,
                error: EnvironmentalErrorClass::DescriptorPressure,
                next_probe,
            });
        }
        self.make_ready_at(file_id, now)?;
        self.poll_inner(
            now,
            Some(ReadLimit {
                file_id,
                end_offset,
            }),
        )
    }

    fn poll_inner(
        &mut self,
        now: Instant,
        limit: Option<ReadLimit>,
    ) -> Result<ReaderPoll, ReaderError> {
        if self.cancellation_requested() {
            return Ok(ReaderPoll::Cancelled);
        }
        // This is a lock-free poison/integrity check. The process-wide
        // registry mutex remains off the source-byte data path.
        self.lease_scope.ensure_healthy_fast()?;
        if let Some(request) = self.pending_eviction {
            return Ok(ReaderPoll::EvictionRequired(request));
        }
        if self.read_buffer.is_none() {
            return Err(ReaderError::Inconsistent {
                reason: "a source turn is outstanding",
            });
        }
        if limit.is_none() {
            self.activate_due(now)?;
            self.activate_descriptor_pressure_due(now)?;
        }
        let Some(file_id) = self.ready.pop_front() else {
            let descriptor_probe = self.descriptor_pressure.retry_at(now)?;
            return Ok(ReaderPoll::Idle {
                next_probe: match (
                    self.eof_deadlines.first().map(|(deadline, _)| *deadline),
                    descriptor_probe,
                ) {
                    (Some(left), Some(right)) => Some(left.min(right)),
                    (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
                    (None, None) => None,
                },
            });
        };
        if limit.is_some_and(|limit| limit.file_id != file_id) {
            return Err(ReaderError::Inconsistent {
                reason: "bounded drain poll selected a different reader",
            });
        }
        {
            let reader = self
                .readers
                .get_mut(&file_id)
                .ok_or(ReaderError::Inconsistent {
                    reason: "ready queue points to a missing reader",
                })?;
            if !matches!(reader.schedule, ScheduleState::Ready) {
                return Err(ReaderError::Inconsistent {
                    reason: "ready queue contains a reader that is not ready",
                });
            }
            reader.schedule = ScheduleState::Paused;
            if !reader.present && reader.resident.is_none() {
                return Ok(ReaderPoll::RemovedWithoutDescriptor { file_id });
            }
        }

        if self
            .readers
            .get(&file_id)
            .is_none_or(|reader| reader.resident.is_none())
        {
            if self.open_count >= self.settings.max_open_files {
                if let Some(victim_file_id) = self.select_lrs_victim(file_id) {
                    let request = self.new_eviction_request(file_id, victim_file_id)?;
                    self.readers
                        .get_mut(&file_id)
                        .ok_or(ReaderError::Inconsistent {
                            reason: "eviction target disappeared",
                        })?
                        .schedule = ScheduleState::Ready;
                    self.ready.push_front(file_id);
                    self.pending_eviction = Some(request);
                    return Ok(ReaderPoll::EvictionRequired(request));
                }
                self.readers
                    .get_mut(&file_id)
                    .ok_or(ReaderError::Inconsistent {
                        reason: "descriptor-blocked target disappeared",
                    })?
                    .schedule = ScheduleState::DescriptorBlocked;
                if !self.descriptor_blocked.insert(file_id) {
                    return Err(ReaderError::Inconsistent {
                        reason: "descriptor-blocked reader was already indexed",
                    });
                }
                return Ok(ReaderPoll::DescriptorCapacityBlocked { file_id });
            }
            if let Some(next_probe) = self.descriptor_pressure_deadline(now)? {
                return self.schedule_descriptor_pressure_waiter(
                    file_id,
                    next_probe,
                    EnvironmentalOperation::Open,
                );
            }
            let opened = self.open_reader_for_poll(file_id);
            if self.cancellation_requested() {
                return Ok(ReaderPoll::Cancelled);
            }
            match opened {
                Ok(None) => return Ok(ReaderPoll::Cancelled),
                Ok(Some(OpenReaderOutcome::Compatible)) => {
                    self.clear_environmental_success(file_id, Some(now))?;
                }
                Ok(Some(OpenReaderOutcome::Truncated {
                    observed_size,
                    observed_fingerprint,
                })) => {
                    self.clear_environmental_success(file_id, Some(now))?;
                    let reader = self
                        .readers
                        .get(&file_id)
                        .ok_or(ReaderError::Inconsistent {
                            reason: "truncated reopen target disappeared",
                        })?;
                    return Ok(ReaderPoll::Truncated {
                        file_id,
                        file_epoch: reader.file_epoch,
                        committed_offset: reader.committed_offset,
                        read_offset: reader.read_offset,
                        observed_size,
                        observed_fingerprint,
                    });
                }
                Err(ReaderError::Reopen {
                    source: IdentityError::CandidateChangedDuringIdentity { .. },
                    ..
                }) => {
                    let next_probe = self.schedule_eof_probe(file_id, now)?;
                    return Ok(ReaderPoll::EvidenceUnstable {
                        file_id,
                        next_probe,
                    });
                }
                Err(ref error)
                    if environmental_reader_error(error)
                        .is_some_and(|_| !self.cancellation_requested()) =>
                {
                    let (operation, class) =
                        environmental_reader_error(error).ok_or(ReaderError::Inconsistent {
                            reason: "environmental reopen classification disappeared",
                        })?;
                    return self.schedule_environmental_backoff(file_id, now, operation, class);
                }
                Err(ReaderError::Reopen { .. }) => {
                    let reader =
                        self.readers
                            .get_mut(&file_id)
                            .ok_or(ReaderError::Inconsistent {
                                reason: "incompatible reopen target disappeared",
                            })?;
                    if !reader.present {
                        return Err(ReaderError::Inconsistent {
                            reason: "incompatible reopen target was already removed",
                        });
                    }
                    reader.present = false;
                    reader.schedule = ScheduleState::Paused;
                    self.removed_count =
                        self.removed_count
                            .checked_add(1)
                            .ok_or(ReaderError::CounterOverflow {
                                counter: "removed readers",
                            })?;
                    return Ok(ReaderPoll::RemovedWithoutDescriptor { file_id });
                }
                Err(error) => {
                    self.readers
                        .get_mut(&file_id)
                        .ok_or(ReaderError::Inconsistent {
                            reason: "failed reopen target disappeared",
                        })?
                        .schedule = ScheduleState::Paused;
                    return Err(error);
                }
            }
        }

        if self.cancellation_requested() {
            return Ok(ReaderPoll::Cancelled);
        }
        let service_sequence = increment(&mut self.service_sequence, "reader service sequence")?;
        let (file_epoch, committed_offset, framing_resume, source_offset, diagnostic_path) = {
            let reader = self
                .readers
                .get_mut(&file_id)
                .ok_or(ReaderError::Inconsistent {
                    reason: "selected reader disappeared",
                })?;
            let resident = reader.resident.as_mut().ok_or(ReaderError::Inconsistent {
                reason: "selected reader has no descriptor",
            })?;
            resident.last_served_sequence = service_sequence;
            (
                reader.file_epoch,
                reader.committed_offset,
                reader.framing_resume,
                reader.read_offset,
                reader.matched_path.clone(),
            )
        };

        let mut buffer = self.read_buffer.take().ok_or(ReaderError::Inconsistent {
            reason: "source buffer disappeared before a read",
        })?;
        let turn_bytes = match limit {
            Some(limit) => {
                let remaining = limit.end_offset.checked_sub(source_offset).ok_or(
                    ReaderError::InvalidProgress {
                        file_id,
                        reason: "drain frontier precedes the current read offset",
                    },
                )?;
                if remaining == 0 {
                    self.read_buffer = Some(buffer);
                    return Err(ReaderError::InvalidProgress {
                        file_id,
                        reason: "drain poll requested a turn at its exact frontier",
                    });
                }
                usize::try_from(remaining)
                    .unwrap_or(usize::MAX)
                    .min(self.settings.max_read_bytes_per_turn)
            }
            None => self.settings.max_read_bytes_per_turn,
        };
        buffer.resize(turn_bytes, 0);
        #[cfg(test)]
        if let Some(gate) = self.next_source_read_gate.take() {
            gate.block();
        }
        if self.cancellation_requested() {
            self.read_buffer = Some(buffer);
            return Ok(ReaderPoll::Cancelled);
        }
        let _read_turns = increment(&mut self.counters.read_turns, "source read turns")?;
        #[cfg(test)]
        let read_result = if let Some(source) = self.fail_next_read.take() {
            Err(source)
        } else {
            let reader = self
                .readers
                .get(&file_id)
                .ok_or(ReaderError::Inconsistent {
                    reason: "selected reader disappeared before a read",
                })?;
            let resident = reader.resident.as_ref().ok_or(ReaderError::Inconsistent {
                reason: "selected reader descriptor disappeared before a read",
            })?;
            read_source_at_cancellable(&resident.file, source_offset, &mut buffer, &mut || {
                self.cancellation_requested()
            })
        };
        #[cfg(not(test))]
        let read_result = {
            let reader = self
                .readers
                .get(&file_id)
                .ok_or(ReaderError::Inconsistent {
                    reason: "selected reader disappeared before a read",
                })?;
            let resident = reader.resident.as_ref().ok_or(ReaderError::Inconsistent {
                reason: "selected reader descriptor disappeared before a read",
            })?;
            read_source_at_cancellable(&resident.file, source_offset, &mut buffer, &mut || {
                self.cancellation_requested()
            })
        };
        if self.cancellation_requested() {
            self.read_buffer = Some(buffer);
            return Ok(ReaderPoll::Cancelled);
        }
        let count = match read_result {
            Ok(Some(count)) => count,
            Ok(None) => {
                self.read_buffer = Some(buffer);
                return Ok(ReaderPoll::Cancelled);
            }
            Err(source) => {
                self.read_buffer = Some(buffer);
                let class = classify_io_error(&source);
                return self.schedule_environmental_backoff(
                    file_id,
                    now,
                    EnvironmentalOperation::Read,
                    class,
                );
            }
        };
        if count != 0 {
            self.clear_environmental_success(file_id, None)?;
        }

        if count == 0 {
            self.read_buffer = Some(buffer);
            if self.cancellation_requested() {
                return Ok(ReaderPoll::Cancelled);
            }
            let observation = {
                let reader = self
                    .readers
                    .get(&file_id)
                    .ok_or(ReaderError::Inconsistent {
                        reason: "EOF reader disappeared before metadata inspection",
                    })?;
                let resident = reader.resident.as_ref().ok_or(ReaderError::Inconsistent {
                    reason: "EOF reader descriptor disappeared before metadata inspection",
                })?;
                let observation = collect_consistent_fingerprint_cancellable(
                    &resident.file,
                    &diagnostic_path,
                    self.settings.fingerprint_bytes,
                    self.settings.ignored_header_bytes,
                    &mut || self.cancellation_requested(),
                );
                classify_cancellable_fingerprint_observation(observation)
            };
            let observation = match observation {
                Ok(Some(observation)) => observation,
                Ok(None) => return Ok(ReaderPoll::Cancelled),
                Err(ref error)
                    if environmental_reader_error(error)
                        .is_some_and(|_| !self.cancellation_requested()) =>
                {
                    let (operation, class) =
                        environmental_reader_error(error).ok_or(ReaderError::Inconsistent {
                            reason: "environmental EOF classification disappeared",
                        })?;
                    return self.schedule_environmental_backoff(file_id, now, operation, class);
                }
                Err(error) => return Err(error),
            };
            if self.cancellation_requested() {
                return Ok(ReaderPoll::Cancelled);
            }
            let (observed_fingerprint, observed_size) = match observation {
                FingerprintObservation::Stable { fingerprint, size } => {
                    self.clear_environmental_success(file_id, None)?;
                    (fingerprint, size)
                }
                FingerprintObservation::Retry => {
                    let next_probe = self.schedule_eof_probe(file_id, now)?;
                    return Ok(ReaderPoll::EvidenceUnstable {
                        file_id,
                        next_probe,
                    });
                }
            };
            let fingerprint_mismatch = {
                let reader = self
                    .readers
                    .get(&file_id)
                    .ok_or(ReaderError::Inconsistent {
                        reason: "EOF reader disappeared after evidence collection",
                    })?;
                !observed_fingerprint.starts_with(&reader.durable_fingerprint)
            };
            let known_continuation_end = self.readers.get(&file_id).and_then(|reader| match reader
                .framing_resume
            {
                FramingResume::Continuation {
                    record_end_offset, ..
                } if record_end_offset != 0 => Some(record_end_offset),
                _ => None,
            });
            if observed_size < source_offset
                || known_continuation_end.is_some_and(|end| observed_size < end)
                || fingerprint_mismatch
            {
                return Ok(ReaderPoll::Truncated {
                    file_id,
                    file_epoch,
                    committed_offset,
                    read_offset: source_offset,
                    observed_size,
                    observed_fingerprint,
                });
            }
            let next_probe = self.schedule_eof_probe(file_id, now)?;
            let _eof_observations =
                increment(&mut self.counters.eof_observations, "EOF observations")?;
            return Ok(ReaderPoll::EndOfFile {
                file_id,
                file_epoch,
                source_offset,
                next_probe,
            });
        }

        add(
            &mut self.counters.source_bytes_read,
            count as u64,
            "source bytes read",
        )?;
        buffer.truncate(count);
        let ticket = increment(&mut self.turn_sequence, "read turn sequence")?;
        self.readers
            .get_mut(&file_id)
            .ok_or(ReaderError::Inconsistent {
                reason: "data reader disappeared",
            })?
            .schedule = ScheduleState::InFlight {
            ticket,
            source_offset,
            available: count,
        };
        Ok(ReaderPoll::Data(ReadTurn {
            ticket,
            file_id,
            file_epoch,
            committed_offset,
            framing_resume,
            source_offset,
            bytes: buffer,
        }))
    }

    /// Returns one source turn and advances only by the caller-consumed
    /// prefix.
    pub(crate) fn complete_turn(
        &mut self,
        mut turn: ReadTurn,
        consumed: usize,
        disposition: TurnDisposition,
    ) -> Result<(), ReaderError> {
        if self.read_buffer.is_some() {
            return Err(ReaderError::InvalidTurn {
                file_id: turn.file_id,
                ticket: turn.ticket,
            });
        }
        let reader = self
            .readers
            .get_mut(&turn.file_id)
            .ok_or(ReaderError::UnknownFile {
                file_id: turn.file_id,
            })?;
        let (ticket, source_offset, available) = match reader.schedule {
            ScheduleState::InFlight {
                ticket,
                source_offset,
                available,
            } => (ticket, source_offset, available),
            _ => {
                return Err(ReaderError::InvalidTurn {
                    file_id: turn.file_id,
                    ticket: turn.ticket,
                });
            }
        };
        if ticket != turn.ticket
            || source_offset != turn.source_offset
            || available != turn.bytes.len()
            || turn.file_epoch != reader.file_epoch
            || turn.source_offset != reader.read_offset
        {
            return Err(ReaderError::InvalidTurn {
                file_id: turn.file_id,
                ticket: turn.ticket,
            });
        }
        if consumed > available {
            let error = ReaderError::InvalidTurnConsumption {
                file_id: turn.file_id,
                consumed,
                available,
            };
            reader.schedule = ScheduleState::Paused;
            turn.bytes.clear();
            turn.bytes.resize(self.settings.max_read_bytes_per_turn, 0);
            self.read_buffer = Some(turn.bytes);
            return Err(error);
        }
        let Some(read_offset) = reader.read_offset.checked_add(consumed as u64) else {
            reader.schedule = ScheduleState::Paused;
            turn.bytes.clear();
            turn.bytes.resize(self.settings.max_read_bytes_per_turn, 0);
            self.read_buffer = Some(turn.bytes);
            return Err(ReaderError::OffsetOverflow {
                file_id: turn.file_id,
            });
        };
        reader.read_offset = read_offset;
        reader.schedule = match disposition {
            TurnDisposition::Ready => ScheduleState::Ready,
            TurnDisposition::Paused => ScheduleState::Paused,
        };
        turn.bytes.clear();
        turn.bytes.resize(self.settings.max_read_bytes_per_turn, 0);
        self.read_buffer = Some(turn.bytes);
        if disposition == TurnDisposition::Ready {
            self.ready.push_back(turn.file_id);
        }
        self.promote_descriptor_waiter()?;
        Ok(())
    }

    /// Makes a paused or EOF reader immediately eligible for a later
    /// round-robin turn.
    pub(crate) fn make_ready(&mut self, file_id: FileId) -> Result<(), ReaderError> {
        self.make_ready_at(file_id, Instant::now())
    }

    fn make_ready_at(&mut self, file_id: FileId, now: Instant) -> Result<(), ReaderError> {
        if self.descriptor_pressure_waiting.contains_key(&file_id)
            && self.descriptor_pressure.retry_at(now)?.is_some()
        {
            return Ok(());
        }
        let descriptor_blocked = matches!(
            self.readers
                .get(&file_id)
                .ok_or(ReaderError::UnknownFile { file_id })?
                .schedule,
            ScheduleState::DescriptorBlocked
        );
        if descriptor_blocked
            && self.open_count >= self.settings.max_open_files
            && self.select_lrs_victim(file_id).is_none()
        {
            return Ok(());
        }
        let prior_deadline = {
            let reader = self
                .readers
                .get_mut(&file_id)
                .ok_or(ReaderError::UnknownFile { file_id })?;
            match reader.schedule {
                ScheduleState::Ready => return Ok(()),
                ScheduleState::Eof { next_probe } => {
                    reader.schedule = ScheduleState::Ready;
                    Some(next_probe)
                }
                ScheduleState::DescriptorBlocked => {
                    if !self.descriptor_blocked.remove(&file_id) {
                        return Err(ReaderError::Inconsistent {
                            reason: "descriptor-blocked reader has no index entry",
                        });
                    }
                    let _ = self.descriptor_pressure_waiting.remove(&file_id);
                    reader.schedule = ScheduleState::Ready;
                    None
                }
                ScheduleState::Paused => {
                    reader.schedule = ScheduleState::Ready;
                    None
                }
                ScheduleState::InFlight { .. } => {
                    return Err(ReaderError::InvalidState {
                        file_id,
                        operation: "make ready",
                        state: reader.schedule.name(),
                    });
                }
            }
        };
        if let Some(deadline) = prior_deadline
            && !self.eof_deadlines.remove(&(deadline, file_id))
        {
            return Err(ReaderError::Inconsistent {
                reason: "EOF reader has no deadline entry",
            });
        }
        if prior_deadline.is_some() {
            let _ = self.environmental_waiting.remove(&file_id);
        }
        self.ready.push_back(file_id);
        Ok(())
    }

    /// Moves an EOF reader's next probe earlier without extending an existing
    /// deadline.
    pub(crate) fn cap_eof_deadline(
        &mut self,
        file_id: FileId,
        deadline: Instant,
    ) -> Result<(), ReaderError> {
        let current = match self
            .readers
            .get(&file_id)
            .ok_or(ReaderError::UnknownFile { file_id })?
            .schedule
        {
            ScheduleState::Eof { next_probe } => next_probe,
            ref state => {
                return Err(ReaderError::InvalidState {
                    file_id,
                    operation: "cap EOF deadline",
                    state: state.name(),
                });
            }
        };
        if deadline >= current {
            return Ok(());
        }
        if self.eof_deadlines.contains(&(deadline, file_id)) {
            return Err(ReaderError::Inconsistent {
                reason: "capped EOF deadline already exists",
            });
        }
        if !self.eof_deadlines.remove(&(current, file_id)) {
            return Err(ReaderError::Inconsistent {
                reason: "EOF reader lacks its prior deadline",
            });
        }
        let inserted = self.eof_deadlines.insert((deadline, file_id));
        debug_assert!(inserted, "preflighted EOF deadline insertion must succeed");
        self.readers
            .get_mut(&file_id)
            .ok_or(ReaderError::Inconsistent {
                reason: "EOF reader disappeared while capping its deadline",
            })?
            .schedule = ScheduleState::Eof {
            next_probe: deadline,
        };
        Ok(())
    }

    /// Pauses a reader without changing its source or durable frontier.
    pub(crate) fn pause(&mut self, file_id: FileId) -> Result<(), ReaderError> {
        if self.pending_eviction.is_some_and(|request| {
            request.target_file_id == file_id || request.victim_file_id == file_id
        }) {
            return Err(ReaderError::InvalidState {
                file_id,
                operation: "pause",
                state: "an eviction decision is pending",
            });
        }
        let prior_state = {
            let reader = self
                .readers
                .get_mut(&file_id)
                .ok_or(ReaderError::UnknownFile { file_id })?;
            if matches!(reader.schedule, ScheduleState::InFlight { .. }) {
                return Err(ReaderError::InvalidState {
                    file_id,
                    operation: "pause",
                    state: reader.schedule.name(),
                });
            }
            std::mem::replace(&mut reader.schedule, ScheduleState::Paused)
        };
        match prior_state {
            ScheduleState::Ready => {
                if !self.batch_pause_order_prepared {
                    self.remove_ready(file_id)?;
                }
            }
            ScheduleState::Eof { next_probe } => {
                if !self.eof_deadlines.remove(&(next_probe, file_id)) {
                    return Err(ReaderError::Inconsistent {
                        reason: "paused EOF reader has no deadline entry",
                    });
                }
                let _ = self.environmental_waiting.remove(&file_id);
            }
            ScheduleState::DescriptorBlocked => {
                if !self.descriptor_blocked.remove(&file_id) {
                    return Err(ReaderError::Inconsistent {
                        reason: "paused descriptor-blocked reader has no index entry",
                    });
                }
                let _ = self.descriptor_pressure_waiting.remove(&file_id);
            }
            ScheduleState::Paused => {}
            ScheduleState::InFlight { .. } => unreachable!("checked before replacement"),
        }
        Ok(())
    }

    /// Pauses one reader and rewinds only its in-memory source frontier to a
    /// provisional batch boundary.
    ///
    /// Stage 11 uses this after discarding every speculative decoder/framer
    /// object at batch seal. The durable committed offset and framing resume
    /// remain unchanged until the matching Ack is persisted. The resident
    /// descriptor and runtime lease are preserved.
    pub(crate) fn rewind_provisional_frontier(
        &mut self,
        file_id: FileId,
        file_epoch: u32,
        target_offset: u64,
    ) -> Result<(), ReaderError> {
        if self.read_buffer.is_none() {
            return Err(ReaderError::InvalidState {
                file_id,
                operation: "rewind provisional frontier",
                state: "a source turn is outstanding",
            });
        }
        if self.pending_eviction.is_some_and(|request| {
            request.target_file_id == file_id || request.victim_file_id == file_id
        }) {
            return Err(ReaderError::InvalidState {
                file_id,
                operation: "rewind provisional frontier",
                state: "an eviction decision is pending",
            });
        }

        let (committed_offset, read_offset, schedule_name) = {
            let reader = self
                .readers
                .get(&file_id)
                .ok_or(ReaderError::UnknownFile { file_id })?;
            if reader.file_epoch != file_epoch {
                return Err(ReaderError::InvalidProgress {
                    file_id,
                    reason: "file epoch does not match the live reader",
                });
            }
            if target_offset < reader.committed_offset {
                return Err(ReaderError::InvalidProgress {
                    file_id,
                    reason: "provisional rewind target precedes durable progress",
                });
            }
            if target_offset > reader.read_offset {
                return Err(ReaderError::InvalidProgress {
                    file_id,
                    reason: "provisional rewind target exceeds bytes consumed in memory",
                });
            }
            if matches!(reader.schedule, ScheduleState::InFlight { .. }) {
                return Err(ReaderError::InvalidState {
                    file_id,
                    operation: "rewind provisional frontier",
                    state: reader.schedule.name(),
                });
            }
            (
                reader.committed_offset,
                reader.read_offset,
                reader.schedule.name(),
            )
        };

        // Validate the scheduling indexes before changing either one, so an
        // integrity failure leaves the reader frontier untouched.
        match &self
            .readers
            .get(&file_id)
            .ok_or(ReaderError::UnknownFile { file_id })?
            .schedule
        {
            ScheduleState::Ready => {
                if self
                    .ready
                    .iter()
                    .filter(|queued| **queued == file_id)
                    .count()
                    != 1
                {
                    return Err(ReaderError::Inconsistent {
                        reason: "ready reader does not have exactly one queue entry",
                    });
                }
            }
            ScheduleState::Eof { next_probe } => {
                if !self.eof_deadlines.contains(&(*next_probe, file_id)) {
                    return Err(ReaderError::Inconsistent {
                        reason: "EOF reader has no deadline entry",
                    });
                }
            }
            ScheduleState::DescriptorBlocked | ScheduleState::Paused => {}
            ScheduleState::InFlight { .. } => {
                return Err(ReaderError::InvalidState {
                    file_id,
                    operation: "rewind provisional frontier",
                    state: schedule_name,
                });
            }
        }
        let rewound = read_offset - target_offset;
        let updated_rewound = self
            .counters
            .source_bytes_rewound
            .checked_add(rewound)
            .ok_or(ReaderError::CounterOverflow {
                counter: "source bytes rewound",
            })?;

        if !(self.batch_pause_order_prepared
            && matches!(
                self.readers
                    .get(&file_id)
                    .ok_or(ReaderError::UnknownFile { file_id })?
                    .schedule,
                ScheduleState::Ready
            ))
        {
            self.remove_scheduling_state(file_id)?;
        }
        let reader = self
            .readers
            .get_mut(&file_id)
            .ok_or(ReaderError::Inconsistent {
                reason: "rewound reader disappeared",
            })?;
        debug_assert_eq!(reader.committed_offset, committed_offset);
        reader.read_offset = target_offset;
        reader.schedule = ScheduleState::Paused;
        reader.paused_for_batch = true;
        self.counters.source_bytes_rewound = updated_rewound;
        Ok(())
    }

    /// Records an Ack-gated durable frontier after the checkpoint store has
    /// accepted the matching progress operation.
    pub(crate) fn observe_committed_progress(
        &mut self,
        file_id: FileId,
        file_epoch: u32,
        committed_offset: u64,
        framing_resume: FramingResume,
    ) -> Result<(), ReaderError> {
        self.preflight_committed_progress(file_id, file_epoch, committed_offset)?;
        self.apply_preflighted_committed_progress(
            file_id,
            file_epoch,
            committed_offset,
            framing_resume,
        );
        Ok(())
    }

    /// Validates an Ack-gated reader transition without changing live state.
    pub(crate) fn preflight_committed_progress(
        &self,
        file_id: FileId,
        file_epoch: u32,
        committed_offset: u64,
    ) -> Result<(), ReaderError> {
        let reader = self
            .readers
            .get(&file_id)
            .ok_or(ReaderError::UnknownFile { file_id })?;
        if file_epoch != reader.file_epoch {
            return Err(ReaderError::InvalidProgress {
                file_id,
                reason: "file epoch does not match the live reader",
            });
        }
        if committed_offset < reader.committed_offset {
            return Err(ReaderError::InvalidProgress {
                file_id,
                reason: "committed offset moved backward",
            });
        }
        if committed_offset > reader.read_offset {
            return Err(ReaderError::InvalidProgress {
                file_id,
                reason: "committed offset exceeds bytes consumed in memory",
            });
        }
        Ok(())
    }

    /// Applies a reader transition already validated against unchanged state.
    pub(crate) fn apply_preflighted_committed_progress(
        &mut self,
        file_id: FileId,
        file_epoch: u32,
        committed_offset: u64,
        framing_resume: FramingResume,
    ) {
        debug_assert!(
            self.preflight_committed_progress(file_id, file_epoch, committed_offset)
                .is_ok()
        );
        let reader = self
            .readers
            .get_mut(&file_id)
            .expect("preflighted reader must remain present");
        reader.committed_offset = committed_offset;
        reader.framing_resume = framing_resume;
        if self
            .pending_eviction
            .is_some_and(|request| request.victim_file_id == file_id)
        {
            self.pending_eviction = None;
        }
    }

    /// Resumes exactly the readers paused by the most recent batch seal.
    ///
    /// Readers admitted while a batch was retained were never marked and
    /// remain in their existing scheduling state.
    #[cfg(test)]
    pub(crate) fn resume_after_batch_commit(&mut self) -> Result<(), ReaderError> {
        self.finish_batch_commit(true)
    }

    /// Clears the exact batch-pause population, optionally making it ready.
    #[cfg(test)]
    pub(crate) fn finish_batch_commit(&mut self, resume: bool) -> Result<(), ReaderError> {
        self.preflight_batch_commit()?;
        self.finish_preflighted_batch_commit(resume);
        Ok(())
    }

    /// Validates the batch-pause population without changing scheduling state.
    pub(crate) fn preflight_batch_commit(&self) -> Result<(), ReaderError> {
        for reader in self.readers.values() {
            if reader.paused_for_batch && !matches!(reader.schedule, ScheduleState::Paused) {
                return Err(ReaderError::Inconsistent {
                    reason: "batch-paused reader is not in the paused schedule state",
                });
            }
        }
        if self.batch_pause_order_prepared
            && (self.ready.len() != self.readers.len()
                || self.ready.iter().any(|file_id| {
                    self.readers.get(file_id).is_none_or(|reader| {
                        !reader.paused_for_batch
                            || !matches!(reader.schedule, ScheduleState::Paused)
                    })
                }))
        {
            return Err(ReaderError::Inconsistent {
                reason: "prepared batch resume order does not cover every paused reader",
            });
        }
        if let Some(file_id) = self.deferred_eviction_target {
            let reader = self
                .readers
                .get(&file_id)
                .ok_or(ReaderError::Inconsistent {
                    reason: "deferred eviction target is missing",
                })?;
            if !reader.paused_for_batch || !matches!(reader.schedule, ScheduleState::Paused) {
                return Err(ReaderError::Inconsistent {
                    reason: "deferred eviction target is not paused for the retained batch",
                });
            }
        }
        Ok(())
    }

    /// Clears a batch-pause population already validated against unchanged state.
    pub(crate) fn finish_preflighted_batch_commit(&mut self, resume: bool) {
        debug_assert!(self.preflight_batch_commit().is_ok());
        let deferred_eviction_target = self.deferred_eviction_target.take();
        let order_prepared = std::mem::take(&mut self.batch_pause_order_prepared);
        for reader in self.readers.values_mut() {
            if reader.paused_for_batch {
                reader.paused_for_batch = false;
                if resume {
                    reader.schedule = ScheduleState::Ready;
                    if order_prepared {
                        continue;
                    }
                    if deferred_eviction_target == Some(reader.file_id) {
                        self.ready.push_front(reader.file_id);
                    } else {
                        self.ready.push_back(reader.file_id);
                    }
                }
            }
        }
        if order_prepared && !resume {
            self.ready.clear();
        }
    }

    /// Captures the current fair scheduler order before a receiver-wide batch
    /// pause rewinds every logical reader.
    pub(crate) fn prepare_batch_pause_order(&mut self) -> Result<(), ReaderError> {
        if self.batch_pause_order_prepared {
            return Err(ReaderError::Inconsistent {
                reason: "batch pause order was already prepared",
            });
        }
        if self
            .readers
            .values()
            .any(|reader| matches!(reader.schedule, ScheduleState::InFlight { .. }))
        {
            return Err(ReaderError::Inconsistent {
                reason: "batch pause order cannot include an in-flight source turn",
            });
        }

        if let Some(file_id) = self.deferred_eviction_target {
            self.ready.push_front(file_id);
        }
        self.ready
            .extend(self.eof_deadlines.iter().map(|(_, file_id)| *file_id));
        self.ready.extend(self.descriptor_blocked.iter().copied());
        self.ready.extend(
            self.readers
                .values()
                .filter(|reader| {
                    matches!(reader.schedule, ScheduleState::Paused)
                        && self.deferred_eviction_target != Some(reader.file_id)
                })
                .map(|reader| reader.file_id),
        );
        if self.ready.len() != self.readers.len() {
            return Err(ReaderError::Inconsistent {
                reason: "scheduler states do not form one complete batch resume order",
            });
        }
        self.batch_pause_order_prepared = true;
        Ok(())
    }

    /// Confirms that caller-owned uncommitted state for the selected victim
    /// has been discarded, then closes only its descriptor and rewinds its
    /// in-memory read frontier.
    pub(crate) fn confirm_eviction(&mut self, request: EvictionRequest) -> Result<(), ReaderError> {
        if self.pending_eviction != Some(request) {
            return Err(ReaderError::InvalidTurn {
                file_id: request.victim_file_id,
                ticket: request.ticket,
            });
        }
        let target = self
            .readers
            .get(&request.target_file_id)
            .ok_or(ReaderError::UnknownFile {
                file_id: request.target_file_id,
            })?;
        if !target.present || target.resident.is_some() {
            return Err(ReaderError::InvalidTurn {
                file_id: request.target_file_id,
                ticket: request.ticket,
            });
        }
        let rewound = {
            let victim =
                self.readers
                    .get_mut(&request.victim_file_id)
                    .ok_or(ReaderError::UnknownFile {
                        file_id: request.victim_file_id,
                    })?;
            if victim.committed_offset != request.committed_offset
                || victim.read_offset != request.read_offset
                || victim.resident.is_none()
                || !victim.present
            {
                return Err(ReaderError::InvalidTurn {
                    file_id: request.victim_file_id,
                    ticket: request.ticket,
                });
            }
            let rewound = victim.read_offset - victim.committed_offset;
            let _closed = victim.resident.take();
            victim.read_offset = victim.committed_offset;
            rewound
        };
        self.open_count = self
            .open_count
            .checked_sub(1)
            .ok_or(ReaderError::Inconsistent {
                reason: "open descriptor count underflowed",
            })?;
        let _descriptor_evictions = increment(
            &mut self.counters.descriptor_evictions,
            "descriptor evictions",
        )?;
        add(
            &mut self.counters.source_bytes_rewound,
            rewound,
            "source bytes rewound",
        )?;
        self.pending_eviction = None;
        Ok(())
    }

    /// Defers one pending eviction and joins its target to the current
    /// batch-pause population so only the matching batch outcome can resume it.
    pub(crate) fn defer_eviction(&mut self, request: EvictionRequest) -> Result<(), ReaderError> {
        if self.pending_eviction != Some(request) {
            return Err(ReaderError::InvalidTurn {
                file_id: request.target_file_id,
                ticket: request.ticket,
            });
        }
        if self
            .readers
            .get(&request.target_file_id)
            .is_some_and(|reader| reader.paused_for_batch)
            || self.deferred_eviction_target.is_some()
        {
            return Err(ReaderError::InvalidState {
                file_id: request.target_file_id,
                operation: "defer eviction",
                state: "already paused for a batch",
            });
        }
        self.pending_eviction = None;
        self.pause(request.target_file_id)?;
        self.readers
            .get_mut(&request.target_file_id)
            .ok_or(ReaderError::Inconsistent {
                reason: "deferred eviction target disappeared",
            })?
            .paused_for_batch = true;
        self.deferred_eviction_target = Some(request.target_file_id);
        Ok(())
    }

    /// Releases a removed logical reader only after later rotation policy has
    /// durably finalized its lifecycle.
    pub(crate) fn release_finalized(&mut self, file_id: FileId) -> Result<Locator, ReaderError> {
        self.cancel_eviction_involving(file_id)?;
        let reader = self
            .readers
            .get(&file_id)
            .ok_or(ReaderError::UnknownFile { file_id })?;
        if reader.present {
            return Err(ReaderError::InvalidState {
                file_id,
                operation: "release finalized reader",
                state: "present",
            });
        }
        if matches!(reader.schedule, ScheduleState::InFlight { .. }) {
            return Err(ReaderError::InvalidState {
                file_id,
                operation: "release finalized reader",
                state: reader.schedule.name(),
            });
        }
        self.release_reader(file_id)
    }

    /// Releases a present or absent logical reader after later policy has
    /// durably quarantined or revoked it.
    pub(crate) fn release_revoked(&mut self, file_id: FileId) -> Result<Locator, ReaderError> {
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_revoked_release) {
            return Err(ReaderError::Inconsistent {
                reason: "injected revoked-reader release failure",
            });
        }
        self.cancel_eviction_involving(file_id)?;
        let reader = self
            .readers
            .get(&file_id)
            .ok_or(ReaderError::UnknownFile { file_id })?;
        if matches!(reader.schedule, ScheduleState::InFlight { .. }) {
            return Err(ReaderError::InvalidState {
                file_id,
                operation: "release revoked reader",
                state: reader.schedule.name(),
            });
        }
        self.release_reader(file_id)
    }

    #[cfg(test)]
    /// Injects one revoked-reader release failure without mutating the table.
    pub(crate) fn fail_next_revoked_release_for_test(&mut self) {
        self.fail_next_revoked_release = true;
    }

    #[cfg(test)]
    /// Blocks the next source read until the returned gate is released.
    pub(crate) fn gate_next_source_read_for_test(&mut self) -> ReaderPollGate {
        let gate = ReaderPollGate::default();
        self.next_source_read_gate = Some(gate.clone());
        gate
    }

    #[cfg(test)]
    /// Makes the next descriptor open fail with the supplied OS error.
    pub(crate) fn fail_next_open_for_test(&mut self, error: test_io::Error) {
        self.fail_next_open = Some(error);
    }

    #[cfg(test)]
    /// Makes the next positioned source read fail with the supplied OS error.
    pub(crate) fn fail_next_read_for_test(&mut self, error: test_io::Error) {
        self.fail_next_read = Some(error);
    }

    #[cfg(test)]
    /// Blocks the next candidate-evidence refresh after its first observation.
    pub(crate) fn gate_next_evidence_refresh_after_first_sample_for_test(&self) -> ReaderPollGate {
        let gate = ReaderPollGate::default();
        *self
            .next_evidence_refresh_gate
            .lock()
            .expect("evidence refresh gate lock poisoned") = Some(gate.clone());
        gate
    }

    /// Validates a durable truncate reset against unchanged live state.
    pub(crate) fn preflight_truncate_reset(
        &self,
        file_id: FileId,
        expected_file_epoch: u32,
        expected_committed_offset: u64,
    ) -> Result<(), ReaderError> {
        self.preflight_lifecycle_transition(file_id)?;
        let reader = self
            .readers
            .get(&file_id)
            .ok_or(ReaderError::UnknownFile { file_id })?;
        if reader.file_epoch != expected_file_epoch {
            return Err(ReaderError::InvalidProgress {
                file_id,
                reason: "truncate reset epoch does not match the live reader",
            });
        }
        if reader.committed_offset != expected_committed_offset {
            return Err(ReaderError::InvalidProgress {
                file_id,
                reason: "truncate reset offset does not match durable progress",
            });
        }
        if !reader.present {
            return Err(ReaderError::InvalidState {
                file_id,
                operation: "reset after truncation",
                state: "removed",
            });
        }
        Ok(())
    }

    /// Applies a truncate reset already persisted under the exact preflighted
    /// epoch and offset.
    pub(crate) fn apply_preflighted_truncate_reset(
        &mut self,
        file_id: FileId,
        expected_file_epoch: u32,
        expected_committed_offset: u64,
        resulting_epoch: u32,
        fingerprint: Vec<u8>,
        resume: bool,
    ) -> Result<(), ReaderError> {
        debug_assert!(
            self.preflight_truncate_reset(file_id, expected_file_epoch, expected_committed_offset)
                .is_ok()
        );
        self.remove_scheduling_state(file_id)?;
        let reader = self
            .readers
            .get_mut(&file_id)
            .ok_or(ReaderError::Inconsistent {
                reason: "preflighted truncate-reset reader disappeared",
            })?;
        reader.file_epoch = resulting_epoch;
        reader.committed_offset = 0;
        reader.read_offset = 0;
        reader.framing_resume = FramingResume::Clean;
        reader.committed_frontier_guard = CommittedFrontierGuard::empty();
        reader.committed_frontier_window = Some(CommittedFrontierWindow::empty());
        reader.durable_fingerprint = fingerprint;
        reader.paused_for_batch = false;
        reader.schedule = if resume {
            ScheduleState::Ready
        } else {
            ScheduleState::Paused
        };
        if resume {
            self.ready.push_back(file_id);
        }
        Ok(())
    }

    /// Validates that a removed reader can be released after durable
    /// finalization without discovering a stale scheduling index afterward.
    pub(crate) fn preflight_release_finalized(&self, file_id: FileId) -> Result<(), ReaderError> {
        self.preflight_lifecycle_transition(file_id)?;
        if self
            .readers
            .get(&file_id)
            .ok_or(ReaderError::UnknownFile { file_id })?
            .present
        {
            return Err(ReaderError::InvalidState {
                file_id,
                operation: "release finalized reader",
                state: "present",
            });
        }
        Ok(())
    }

    /// Validates that an active or removed reader can be released after a
    /// durable quarantine transition.
    pub(crate) fn preflight_release_revoked(&self, file_id: FileId) -> Result<(), ReaderError> {
        self.preflight_lifecycle_transition(file_id)
    }

    /// Returns a snapshot of bounded populations and monotonic activity.
    #[must_use]
    pub(crate) fn stats(&self) -> ReaderStats {
        self.stats_at(Instant::now())
    }

    fn stats_at(&self, now: Instant) -> ReaderStats {
        ReaderStats {
            tracked_readers: self.readers.len(),
            open_files: self.open_count,
            ready_readers: self.ready.len(),
            eof_readers: self
                .eof_deadlines
                .len()
                .saturating_sub(self.environmental_waiting.len()),
            environmental_backoff_readers: self
                .environmental_waiting
                .len()
                .saturating_add(self.descriptor_pressure_waiting.len()),
            removed_readers: self.removed_count,
            descriptor_blocked_readers: self.descriptor_blocked.len(),
            read_turns: self.counters.read_turns,
            source_bytes_read: self.counters.source_bytes_read,
            opens: self.counters.opens,
            reopens: self.counters.reopens,
            descriptor_evictions: self.counters.descriptor_evictions,
            descriptor_reopen_failures: self.counters.descriptor_reopen_failures,
            source_bytes_rewound: self.counters.source_bytes_rewound,
            eof_observations: self.counters.eof_observations,
            eof_reprobes: self.counters.eof_reprobes,
            pinned_rotated_handles: self.pinned_rotated.len(),
            pinned_rotated_oldest_age_ns: self.pinned_rotated.first().map_or(0, |(since, _)| {
                u64::try_from(now.saturating_duration_since(*since).as_nanos()).unwrap_or(u64::MAX)
            }),
        }
    }

    /// Active receiver-global descriptor-pressure deadline, if new
    /// admissions and opens must remain paused.
    pub(crate) fn descriptor_pressure_deadline(
        &self,
        now: Instant,
    ) -> Result<Option<Instant>, ReaderError> {
        Ok(self.descriptor_pressure.retry_at(now)?)
    }

    /// Transfers runtime-lease observations without retaining per-file state.
    pub(crate) fn take_lease_observations(&mut self) -> LeaseObservations {
        std::mem::take(&mut self.lease_observations)
    }

    /// Iterates every logical reader without allocating or cloning path
    /// state.
    pub(crate) fn frontiers(&self) -> impl Iterator<Item = ReaderFrontier> + '_ {
        self.readers.values().map(|reader| ReaderFrontier {
            file_id: reader.file_id,
            file_epoch: reader.file_epoch,
            committed_offset: reader.committed_offset,
            read_offset: reader.read_offset,
            framing_resume: reader.framing_resume,
            present: reader.present,
            descriptor_resident: reader.resident.is_some(),
            paused_for_batch: reader.paused_for_batch,
        })
    }

    /// Whether one logical reader still owns `file_id`.
    pub(crate) fn contains_file(&self, file_id: FileId) -> bool {
        self.readers.contains_key(&file_id)
    }

    /// Returns one logical reader's current frontier without allocation.
    pub(crate) fn frontier(&self, file_id: FileId) -> Result<ReaderFrontier, ReaderError> {
        let reader = self
            .readers
            .get(&file_id)
            .ok_or(ReaderError::UnknownFile { file_id })?;
        Ok(ReaderFrontier {
            file_id: reader.file_id,
            file_epoch: reader.file_epoch,
            committed_offset: reader.committed_offset,
            read_offset: reader.read_offset,
            framing_resume: reader.framing_resume,
            present: reader.present,
            descriptor_resident: reader.resident.is_some(),
            paused_for_batch: reader.paused_for_batch,
        })
    }

    /// Borrows the matched and resolved paths owned by one logical reader.
    pub(crate) fn record_context(
        &self,
        file_id: FileId,
    ) -> Result<ReaderRecordContext<'_>, ReaderError> {
        let reader = self
            .readers
            .get(&file_id)
            .ok_or(ReaderError::UnknownFile { file_id })?;
        Ok(ReaderRecordContext {
            matched_path: &reader.matched_path,
            resolved_path: &reader.resolved_path,
        })
    }

    /// Resolves a currently tracked runtime locator to its durable identity.
    pub(crate) fn file_id_for_locator(&self, locator: Locator) -> Result<FileId, ReaderError> {
        self.by_locator
            .get(&locator)
            .copied()
            .ok_or(ReaderError::UnknownLocator { locator })
    }

    /// Returns the runtime locator owned by one live logical reader.
    pub(crate) fn locator(&self, file_id: FileId) -> Result<Locator, ReaderError> {
        self.readers
            .get(&file_id)
            .map(|reader| reader.locator)
            .ok_or(ReaderError::UnknownFile { file_id })
    }

    /// Returns the exact real committed-frontier window this reader already
    /// owns, ending at `committed_offset`: a bounded in-memory clone with no
    /// filesystem I/O.
    ///
    /// This is the seed used to construct a fresh [`Framer`] after Ack, Nack
    /// rewind, batch seal, descriptor eviction, drain rewind, or carry-over,
    /// so its rolling checkpoint window is real evidence from the moment of
    /// construction, never a fabricated placeholder and never a post-Ack
    /// reread. The window is established once, either at admission (a
    /// genuinely new identity's own evidence) or at the first
    /// (re)validated descriptor open (an existing identity's real window
    /// read and checked against its durable guard); callers only invoke
    /// this once a read has already been served for the file in this
    /// worker lifetime, by which point the window is always present.
    ///
    /// [`Framer`]: crate::receivers::filelog_receiver::framing::Framer
    pub(crate) fn committed_frontier_window(
        &self,
        file_id: FileId,
        committed_offset: u64,
    ) -> Result<CommittedFrontierWindow, ReaderError> {
        let reader = self
            .readers
            .get(&file_id)
            .ok_or(ReaderError::UnknownFile { file_id })?;
        let window =
            reader
                .committed_frontier_window
                .as_ref()
                .ok_or(ReaderError::InvalidState {
                    file_id,
                    operation: "read committed-frontier seed window",
                    state: "committed-frontier window is not yet established",
                })?;
        if window.end_offset() != committed_offset {
            return Err(ReaderError::Inconsistent {
                reason: "retained committed-frontier window does not match the requested offset",
            });
        }
        Ok(window.clone())
    }

    /// Installs the exact real committed-frontier window resulting from a
    /// committed Ack, once the checkpoint append/apply has already
    /// succeeded and the matching offset/resume advance has been applied.
    ///
    /// The caller supplies `None` for a zero-delta or finalize-only update
    /// (the offset does not change), which leaves the retained window
    /// unchanged bit-for-bit; it supplies `Some` only when real progress
    /// happened, with the exact window the batching pipeline already owns
    /// for the new offset.
    pub(crate) fn install_committed_frontier_window(
        &mut self,
        file_id: FileId,
        window: Option<CommittedFrontierWindow>,
    ) -> Result<(), ReaderError> {
        let Some(window) = window else {
            return Ok(());
        };
        let reader = self
            .readers
            .get_mut(&file_id)
            .ok_or(ReaderError::UnknownFile { file_id })?;
        if window.end_offset() != reader.committed_offset {
            return Err(ReaderError::Inconsistent {
                reason: "installed committed-frontier window does not end at the committed offset",
            });
        }
        let guard = window.guard().map_err(|_| ReaderError::Inconsistent {
            reason: "installed committed-frontier window failed to produce a guard",
        })?;
        reader.committed_frontier_guard = guard;
        reader.committed_frontier_window = Some(window);
        Ok(())
    }

    /// Refreshes one queued `Updated` event from the retained native handle
    /// so asynchronous discovery sampling cannot lag the worker frontier.
    pub(crate) fn refresh_candidate_evidence(
        &self,
        candidate: &mut DiscoveredCandidate,
    ) -> Result<CandidateEvidenceRefresh, ReaderError> {
        let file_id = self.file_id_for_locator(candidate.evidence.locator)?;
        let reader = self
            .readers
            .get(&file_id)
            .ok_or(ReaderError::Inconsistent {
                reason: "locator index points to a missing evidence-refresh reader",
            })?;
        if !reader.present {
            return Err(ReaderError::InvalidState {
                file_id,
                operation: "refresh candidate evidence",
                state: "removed",
            });
        }
        let Some(resident) = reader.resident.as_ref() else {
            return Ok(CandidateEvidenceRefresh::DescriptorAbsent);
        };
        #[cfg(not(test))]
        let observation = collect_consistent_fingerprint_cancellable(
            &resident.file,
            &candidate.matched_path,
            self.settings.fingerprint_bytes,
            self.settings.ignored_header_bytes,
            &mut || self.cancellation_requested(),
        );
        #[cfg(test)]
        let observation = collect_consistent_fingerprint_cancellable_with_hook(
            &resident.file,
            &candidate.matched_path,
            self.settings.fingerprint_bytes,
            self.settings.ignored_header_bytes,
            &mut || self.cancellation_requested(),
            || {
                let gate = self
                    .next_evidence_refresh_gate
                    .lock()
                    .expect("evidence refresh gate lock poisoned")
                    .take();
                if let Some(gate) = gate {
                    gate.block();
                }
            },
        );
        if self.cancellation_requested() {
            return Ok(CandidateEvidenceRefresh::Cancelled);
        }
        let Some(observation) = classify_cancellable_fingerprint_observation(observation)? else {
            return Ok(CandidateEvidenceRefresh::Cancelled);
        };
        match observation {
            FingerprintObservation::Stable { fingerprint, size } => {
                candidate.evidence.fingerprint = fingerprint;
                candidate.evidence.size = size;
                Ok(CandidateEvidenceRefresh::Refreshed)
            }
            FingerprintObservation::Retry => Ok(CandidateEvidenceRefresh::Retry),
        }
    }

    /// Borrows the durable identity evidence associated with a live locator.
    pub(crate) fn identity_context(
        &self,
        locator: Locator,
    ) -> Result<ReaderIdentityContext<'_>, ReaderError> {
        let file_id = self.file_id_for_locator(locator)?;
        let reader = self
            .readers
            .get(&file_id)
            .ok_or(ReaderError::Inconsistent {
                reason: "locator index points to a missing reader",
            })?;
        Ok(ReaderIdentityContext {
            file_id,
            file_epoch: reader.file_epoch,
            committed_offset: reader.committed_offset,
            read_offset: reader.read_offset,
            durable_fingerprint: &reader.durable_fingerprint,
        })
    }

    /// Releases all runtime leases and unregisters the receiver scope.
    ///
    /// The future worker calls this only after stopping reads and resolving
    /// any retained batch according to drain or forced-shutdown policy.
    pub(crate) fn shutdown(mut self) -> Result<(), ReaderError> {
        let mut first_error = None;
        for (_, reader) in self.readers.drain() {
            if let Err(error) = reader.lease.release()
                && first_error.is_none()
            {
                first_error = Some(ReaderError::Lease(error));
            }
        }
        self.by_locator.clear();
        self.ready.clear();
        self.eof_deadlines.clear();
        self.environmental_waiting.clear();
        self.environmental_failures.clear();
        if let Err(error) = self.descriptor_pressure.reset()
            && first_error.is_none()
        {
            first_error = Some(ReaderError::DescriptorPressure(error));
        }
        self.descriptor_pressure_waiting.clear();
        self.descriptor_blocked.clear();
        self.pinned_rotated.clear();
        self.open_count = 0;
        self.removed_count = 0;
        if let Err(error) = self.lease_scope.close()
            && first_error.is_none()
        {
            first_error = Some(ReaderError::Lease(error));
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn activate_due(&mut self, now: Instant) -> Result<(), ReaderError> {
        while let Some((deadline, file_id)) = self.eof_deadlines.first().copied() {
            if deadline > now {
                break;
            }
            if !self.eof_deadlines.remove(&(deadline, file_id)) {
                return Err(ReaderError::Inconsistent {
                    reason: "due EOF deadline disappeared",
                });
            }
            let reader = self
                .readers
                .get_mut(&file_id)
                .ok_or(ReaderError::Inconsistent {
                    reason: "EOF deadline points to a missing reader",
                })?;
            if !matches!(
                reader.schedule,
                ScheduleState::Eof { next_probe } if next_probe == deadline
            ) {
                return Err(ReaderError::Inconsistent {
                    reason: "EOF deadline and reader state disagree",
                });
            }
            reader.schedule = ScheduleState::Ready;
            let environmental = self.environmental_waiting.remove(&file_id);
            if !environmental {
                let _ = increment(&mut self.counters.eof_reprobes, "EOF reprobes")?;
            }
            self.ready.push_back(file_id);
        }
        Ok(())
    }

    fn activate_descriptor_pressure_due(&mut self, now: Instant) -> Result<(), ReaderError> {
        if self
            .descriptor_pressure
            .current()?
            .is_some_and(|state| state.retry_at() > now)
        {
            return Ok(());
        }
        while let Some(file_id) = self.descriptor_pressure_waiting.keys().next().copied() {
            let _ = self.descriptor_pressure_waiting.remove(&file_id);
            if !self.descriptor_blocked.remove(&file_id) {
                return Err(ReaderError::Inconsistent {
                    reason: "descriptor-pressure reader has no blocked index entry",
                });
            }
            let reader = self
                .readers
                .get_mut(&file_id)
                .ok_or(ReaderError::Inconsistent {
                    reason: "descriptor-pressure deadline points to a missing reader",
                })?;
            if !matches!(reader.schedule, ScheduleState::DescriptorBlocked) {
                return Err(ReaderError::Inconsistent {
                    reason: "descriptor-pressure reader is not blocked",
                });
            }
            reader.schedule = ScheduleState::Ready;
            self.ready.push_back(file_id);
        }
        Ok(())
    }

    fn open_reader_for_poll(
        &mut self,
        file_id: FileId,
    ) -> Result<Option<OpenReaderOutcome>, ReaderError> {
        let is_reopen = self
            .readers
            .get(&file_id)
            .ok_or(ReaderError::UnknownFile { file_id })?
            .ever_opened;
        #[cfg(test)]
        let result = if let Some(source) = self.fail_next_open.take() {
            let path = self
                .readers
                .get(&file_id)
                .ok_or(ReaderError::UnknownFile { file_id })?
                .matched_path
                .clone();
            Err(ReaderError::Reopen {
                file_id,
                source: IdentityError::Io {
                    operation: "open injected reader",
                    path,
                    source,
                },
            })
        } else {
            self.open_reader(file_id)
        };
        #[cfg(not(test))]
        let result = self.open_reader(file_id);
        if is_reopen && result.is_err() {
            let _ = increment(
                &mut self.counters.descriptor_reopen_failures,
                "descriptor reopen failures",
            )?;
        }
        result
    }

    fn open_reader(&mut self, file_id: FileId) -> Result<Option<OpenReaderOutcome>, ReaderError> {
        let (resolved_path, matched_path, locator, durable_fingerprint, read_offset, ever_opened) = {
            let reader = self
                .readers
                .get(&file_id)
                .ok_or(ReaderError::UnknownFile { file_id })?;
            (
                reader.resolved_path.clone(),
                reader.matched_path.clone(),
                reader.locator,
                reader.durable_fingerprint.clone(),
                // A resumed continuation's exact known record end (when
                // present) is a durable promise that at least that many
                // source bytes exist; a reopened handle observably shorter
                // than it must be classified as truncated before any
                // continuation bytes are emitted, exactly like a source
                // shorter than bytes already consumed.
                match reader.framing_resume {
                    FramingResume::Continuation {
                        record_end_offset, ..
                    } if record_end_offset != 0 => reader.read_offset.max(record_end_offset),
                    _ => reader.read_offset,
                },
                reader.ever_opened,
            )
        };
        let reopened = reopen_candidate_at_cancellable(
            &resolved_path,
            &matched_path,
            self.settings.follow_symlinks,
            self.settings.fingerprint_bytes,
            self.settings.ignored_header_bytes,
            locator,
            &durable_fingerprint,
            read_offset,
            || self.cancellation_requested(),
        )
        .map_err(|source| ReaderError::Reopen { file_id, source })?;
        let Some(reopened) = reopened else {
            return Ok(None);
        };
        let (opened, outcome) = match reopened {
            ReopenCandidate::Compatible(opened) => (opened, OpenReaderOutcome::Compatible),
            ReopenCandidate::Truncated(opened) => {
                let outcome = OpenReaderOutcome::Truncated {
                    observed_size: opened.evidence.size,
                    observed_fingerprint: opened.evidence.fingerprint.clone(),
                };
                (opened, outcome)
            }
        };
        // A truncated outcome is about to reset the reader's committed
        // frontier (and its window) through the truncate-reset path, so the
        // stale window never needs revalidation here. Every compatible
        // nonzero frontier is read from this exact validated handle and
        // checked against the durable guard before serving data. Offset zero
        // uses its canonical empty window without filesystem I/O.
        let refreshed_window = if matches!(outcome, OpenReaderOutcome::Compatible) {
            let (committed_offset, committed_frontier_guard, needs_refresh) = {
                let reader = self
                    .readers
                    .get(&file_id)
                    .ok_or(ReaderError::Inconsistent {
                        reason: "reopened reader disappeared before frontier validation",
                    })?;
                (
                    reader.committed_offset,
                    reader.committed_frontier_guard,
                    reader.committed_offset != 0 || reader.committed_frontier_window.is_none(),
                )
            };
            if needs_refresh {
                let Some(window) = self.read_and_validate_committed_frontier_window(
                    file_id,
                    &resolved_path,
                    &opened.file,
                    committed_offset,
                    committed_frontier_guard,
                )?
                else {
                    return Ok(None);
                };
                Some(window)
            } else {
                None
            }
        } else {
            None
        };
        let reader = self
            .readers
            .get_mut(&file_id)
            .ok_or(ReaderError::Inconsistent {
                reason: "reopened reader disappeared",
            })?;
        if reader.resident.is_some() {
            return Err(ReaderError::Inconsistent {
                reason: "reopened reader already has a descriptor",
            });
        }
        reader.resident = Some(ResidentReader {
            file: opened.file,
            last_served_sequence: 0,
        });
        reader.ever_opened = true;
        if let Some(window) = refreshed_window {
            reader.committed_frontier_window = Some(window);
        }
        self.open_count = self
            .open_count
            .checked_add(1)
            .ok_or(ReaderError::Inconsistent {
                reason: "open descriptor count overflowed",
            })?;
        if ever_opened {
            let _reopens = increment(&mut self.counters.reopens, "reader reopens")?;
        } else {
            let _opens = increment(&mut self.counters.opens, "reader opens")?;
        }
        Ok(Some(outcome))
    }

    /// Reads the exact real committed-frontier window ending at
    /// `committed_offset` from `file` (a just-(re)opened, identity-validated
    /// handle) and checks its computed guard against `expected_guard`.
    ///
    /// Returns `Ok(None)` only on lifecycle cancellation. A window whose
    /// guard does not match durable evidence is classified through the same
    /// reopen-mismatch path as an incompatible fingerprint or locator: the
    /// reader is never allowed to silently resume from evidence that does
    /// not match what was durably recorded.
    fn read_and_validate_committed_frontier_window(
        &self,
        file_id: FileId,
        path: &Path,
        file: &std::fs::File,
        committed_offset: u64,
        expected_guard: CommittedFrontierGuard,
    ) -> Result<Option<CommittedFrontierWindow>, ReaderError> {
        let window_len = committed_offset.min(u64::from(
            super::checkpoint::primitives::COMMITTED_FRONTIER_GUARD_WINDOW_BYTES,
        )) as usize;
        let offset = committed_offset - window_len as u64;
        let Some(bytes) = read_fingerprint_cancellable(file, offset, window_len, &mut || {
            self.cancellation_requested()
        })
        .map_err(|source| ReaderError::Read {
            file_id,
            path: path.to_path_buf(),
            source,
        })?
        else {
            return Ok(None);
        };
        if bytes.len() != window_len {
            return Err(ReaderError::Reopen {
                file_id,
                source: IdentityError::ReopenFrontierGuardMismatch {
                    path: path.to_path_buf(),
                },
            });
        }
        let window = CommittedFrontierWindow::new(committed_offset, bytes).map_err(|_| {
            ReaderError::Inconsistent {
                reason: "committed-frontier window length does not match its offset",
            }
        })?;
        let guard = window.guard().map_err(|_| ReaderError::Inconsistent {
            reason: "committed-frontier window failed to produce a guard",
        })?;
        if guard != expected_guard {
            return Err(ReaderError::Reopen {
                file_id,
                source: IdentityError::ReopenFrontierGuardMismatch {
                    path: path.to_path_buf(),
                },
            });
        }
        Ok(Some(window))
    }

    fn select_lrs_victim(&self, target_file_id: FileId) -> Option<FileId> {
        self.readers
            .values()
            .filter(|reader| {
                reader.file_id != target_file_id
                    && reader.present
                    && reader.resident.is_some()
                    && !matches!(reader.schedule, ScheduleState::InFlight { .. })
            })
            .min_by_key(|reader| {
                (
                    reader
                        .resident
                        .as_ref()
                        .map_or(u64::MAX, |resident| resident.last_served_sequence),
                    reader.file_id,
                )
            })
            .map(|reader| reader.file_id)
    }

    fn new_eviction_request(
        &mut self,
        target_file_id: FileId,
        victim_file_id: FileId,
    ) -> Result<EvictionRequest, ReaderError> {
        let victim = self
            .readers
            .get(&victim_file_id)
            .ok_or(ReaderError::UnknownFile {
                file_id: victim_file_id,
            })?;
        Ok(EvictionRequest {
            ticket: increment(&mut self.eviction_sequence, "eviction sequence")?,
            target_file_id,
            victim_file_id,
            committed_offset: victim.committed_offset,
            read_offset: victim.read_offset,
        })
    }

    fn remove_ready(&mut self, file_id: FileId) -> Result<(), ReaderError> {
        let Some(position) = self.ready.iter().position(|queued| *queued == file_id) else {
            return Err(ReaderError::Inconsistent {
                reason: "ready reader has no queue entry",
            });
        };
        let removed = self.ready.remove(position);
        if removed != Some(file_id) {
            return Err(ReaderError::Inconsistent {
                reason: "ready queue removed the wrong reader",
            });
        }
        Ok(())
    }

    fn remove_scheduling_state(&mut self, file_id: FileId) -> Result<(), ReaderError> {
        let state = &self
            .readers
            .get(&file_id)
            .ok_or(ReaderError::UnknownFile { file_id })?
            .schedule;
        match state {
            ScheduleState::Ready => self.remove_ready(file_id),
            ScheduleState::Eof { next_probe } => {
                if self.eof_deadlines.remove(&(*next_probe, file_id)) {
                    let _ = self.environmental_waiting.remove(&file_id);
                    let _ = self.environmental_failures.remove(&file_id);
                    Ok(())
                } else {
                    Err(ReaderError::Inconsistent {
                        reason: "finalized EOF reader has no deadline entry",
                    })
                }
            }
            ScheduleState::DescriptorBlocked => {
                if self.descriptor_blocked.remove(&file_id) {
                    let _ = self.descriptor_pressure_waiting.remove(&file_id);
                    Ok(())
                } else {
                    Err(ReaderError::Inconsistent {
                        reason: "descriptor-blocked reader has no index entry",
                    })
                }
            }
            ScheduleState::Paused => Ok(()),
            ScheduleState::InFlight { .. } => Err(ReaderError::InvalidState {
                file_id,
                operation: "remove scheduling state",
                state: state.name(),
            }),
        }
    }

    fn schedule_eof_probe(
        &mut self,
        file_id: FileId,
        now: Instant,
    ) -> Result<Instant, ReaderError> {
        let next_probe = now
            .checked_add(self.settings.eof_probe_interval)
            .ok_or(ReaderError::DeadlineOverflow)?;
        let state = &self
            .readers
            .get(&file_id)
            .ok_or(ReaderError::Inconsistent {
                reason: "EOF reader disappeared",
            })?
            .schedule;
        if !matches!(state, ScheduleState::Paused) {
            return Err(ReaderError::InvalidState {
                file_id,
                operation: "schedule EOF probe",
                state: state.name(),
            });
        }
        let _ = self.environmental_waiting.remove(&file_id);
        let _ = self.environmental_failures.remove(&file_id);
        self.readers
            .get_mut(&file_id)
            .ok_or(ReaderError::Inconsistent {
                reason: "EOF reader disappeared while scheduling",
            })?
            .schedule = ScheduleState::Eof { next_probe };
        if !self.eof_deadlines.insert((next_probe, file_id)) {
            return Err(ReaderError::Inconsistent {
                reason: "EOF deadline was already present",
            });
        }
        Ok(next_probe)
    }

    fn schedule_environmental_backoff(
        &mut self,
        file_id: FileId,
        now: Instant,
        operation: EnvironmentalOperation,
        error: EnvironmentalErrorClass,
    ) -> Result<ReaderPoll, ReaderError> {
        if error == EnvironmentalErrorClass::DescriptorPressure {
            let retry_at = self.descriptor_pressure.record_failure(now)?;
            return self.schedule_descriptor_pressure_waiter(file_id, retry_at, operation);
        }
        let state = EnvironmentalBackoff::after_failure(
            self.environmental_failures.get(&file_id).copied(),
            now,
        )
        .ok_or(ReaderError::DeadlineOverflow)?;
        let _ = self.environmental_failures.insert(file_id, state);
        self.schedule_environmental_at(file_id, state.retry_at(), operation, error)
    }

    fn schedule_descriptor_pressure_waiter(
        &mut self,
        file_id: FileId,
        next_probe: Instant,
        operation: EnvironmentalOperation,
    ) -> Result<ReaderPoll, ReaderError> {
        let state = &self
            .readers
            .get(&file_id)
            .ok_or(ReaderError::UnknownFile { file_id })?
            .schedule;
        if !matches!(state, ScheduleState::Paused) {
            return Err(ReaderError::InvalidState {
                file_id,
                operation: "wait for descriptor-pressure retry",
                state: state.name(),
            });
        }
        if self.descriptor_blocked.contains(&file_id)
            || self.descriptor_pressure_waiting.contains_key(&file_id)
        {
            return Err(ReaderError::Inconsistent {
                reason: "descriptor-pressure reader was already waiting",
            });
        }
        let blocked_inserted = self.descriptor_blocked.insert(file_id);
        let prior_operation = self.descriptor_pressure_waiting.insert(file_id, operation);
        debug_assert!(blocked_inserted && prior_operation.is_none());
        self.readers
            .get_mut(&file_id)
            .ok_or(ReaderError::Inconsistent {
                reason: "descriptor-pressure reader disappeared",
            })?
            .schedule = ScheduleState::DescriptorBlocked;
        Ok(ReaderPoll::EnvironmentalBackoff {
            file_id,
            operation,
            error: EnvironmentalErrorClass::DescriptorPressure,
            next_probe,
        })
    }

    fn schedule_environmental_at(
        &mut self,
        file_id: FileId,
        next_probe: Instant,
        operation: EnvironmentalOperation,
        error: EnvironmentalErrorClass,
    ) -> Result<ReaderPoll, ReaderError> {
        let state = &self
            .readers
            .get(&file_id)
            .ok_or(ReaderError::UnknownFile { file_id })?
            .schedule;
        if !matches!(state, ScheduleState::Paused) {
            return Err(ReaderError::InvalidState {
                file_id,
                operation: "schedule environmental retry",
                state: state.name(),
            });
        }
        if self.eof_deadlines.contains(&(next_probe, file_id))
            || !self.environmental_waiting.insert(file_id)
        {
            return Err(ReaderError::Inconsistent {
                reason: "environmental retry was already scheduled",
            });
        }
        self.readers
            .get_mut(&file_id)
            .ok_or(ReaderError::Inconsistent {
                reason: "environmental retry reader disappeared",
            })?
            .schedule = ScheduleState::Eof { next_probe };
        if !self.eof_deadlines.insert((next_probe, file_id)) {
            let _ = self.environmental_waiting.remove(&file_id);
            return Err(ReaderError::Inconsistent {
                reason: "environmental retry deadline was already present",
            });
        }
        Ok(ReaderPoll::EnvironmentalBackoff {
            file_id,
            operation,
            error,
            next_probe,
        })
    }

    fn clear_environmental_success(
        &mut self,
        file_id: FileId,
        descriptor_opened_at: Option<Instant>,
    ) -> Result<(), ReaderError> {
        let _ = self.environmental_failures.remove(&file_id);
        if let Some(now) = descriptor_opened_at {
            // The reviewed telemetry contract records reprobes, not a
            // separate recovery instrument.
            let _ = self.descriptor_pressure.clear_after_success(now)?;
        }
        Ok(())
    }

    fn preflight_lifecycle_transition(&self, file_id: FileId) -> Result<(), ReaderError> {
        if self.read_buffer.is_none() {
            return Err(ReaderError::InvalidState {
                file_id,
                operation: "change lifecycle",
                state: "a source turn is outstanding",
            });
        }
        if self.pending_eviction.is_some_and(|request| {
            request.target_file_id == file_id || request.victim_file_id == file_id
        }) {
            return Err(ReaderError::InvalidState {
                file_id,
                operation: "change lifecycle",
                state: "an eviction decision is pending",
            });
        }
        let reader = self
            .readers
            .get(&file_id)
            .ok_or(ReaderError::UnknownFile { file_id })?;
        match reader.schedule {
            ScheduleState::Ready => {
                if self
                    .ready
                    .iter()
                    .filter(|queued| **queued == file_id)
                    .count()
                    != 1
                {
                    return Err(ReaderError::Inconsistent {
                        reason: "ready lifecycle-transition reader lacks one queue entry",
                    });
                }
            }
            ScheduleState::Eof { next_probe } => {
                if !self.eof_deadlines.contains(&(next_probe, file_id)) {
                    return Err(ReaderError::Inconsistent {
                        reason: "EOF lifecycle-transition reader lacks its deadline",
                    });
                }
            }
            ScheduleState::DescriptorBlocked | ScheduleState::Paused => {}
            ScheduleState::InFlight { .. } => {
                return Err(ReaderError::InvalidState {
                    file_id,
                    operation: "change lifecycle",
                    state: reader.schedule.name(),
                });
            }
        }
        Ok(())
    }

    fn cancel_eviction_involving(&mut self, file_id: FileId) -> Result<(), ReaderError> {
        let Some(request) = self.pending_eviction else {
            return Ok(());
        };
        if request.target_file_id != file_id && request.victim_file_id != file_id {
            return Ok(());
        }
        self.pending_eviction = None;
        if request.target_file_id == file_id {
            self.pause(file_id)?;
        }
        Ok(())
    }

    fn promote_descriptor_waiter(&mut self) -> Result<(), ReaderError> {
        let next = self
            .descriptor_blocked
            .iter()
            .copied()
            .find(|file_id| !self.descriptor_pressure_waiting.contains_key(file_id));
        if let Some(file_id) = next
            && (self.open_count < self.settings.max_open_files
                || self.select_lrs_victim(file_id).is_some())
        {
            let reader = self
                .readers
                .get_mut(&file_id)
                .ok_or(ReaderError::Inconsistent {
                    reason: "descriptor waiter disappeared",
                })?;
            if !matches!(reader.schedule, ScheduleState::DescriptorBlocked)
                || !self.descriptor_blocked.remove(&file_id)
            {
                return Err(ReaderError::Inconsistent {
                    reason: "descriptor waiter index and schedule disagree",
                });
            }
            reader.schedule = ScheduleState::Ready;
            self.ready.push_back(file_id);
        }
        Ok(())
    }

    fn release_reader(&mut self, file_id: FileId) -> Result<Locator, ReaderError> {
        self.remove_scheduling_state(file_id)?;
        let _ = self.environmental_waiting.remove(&file_id);
        let _ = self.environmental_failures.remove(&file_id);
        let _ = self.descriptor_pressure_waiting.remove(&file_id);
        let reader = self
            .readers
            .remove(&file_id)
            .ok_or(ReaderError::Inconsistent {
                reason: "released reader disappeared",
            })?;
        if let Some(pinned_since) = reader.pinned_since
            && !self.pinned_rotated.remove(&(pinned_since, file_id))
        {
            return Err(ReaderError::Inconsistent {
                reason: "released pinned reader lacks its age index",
            });
        }
        if reader.resident.is_some() {
            self.open_count = self
                .open_count
                .checked_sub(1)
                .ok_or(ReaderError::Inconsistent {
                    reason: "open descriptor count underflowed during release",
                })?;
        }
        if !reader.present {
            self.removed_count =
                self.removed_count
                    .checked_sub(1)
                    .ok_or(ReaderError::Inconsistent {
                        reason: "removed reader count underflowed during release",
                    })?;
        }
        if self.by_locator.remove(&reader.locator) != Some(file_id) {
            return Err(ReaderError::Inconsistent {
                reason: "released reader locator index disagrees",
            });
        }
        let locator = reader.locator;
        reader.lease.release()?;
        self.promote_descriptor_waiter()?;
        Ok(locator)
    }

    #[cfg(test)]
    fn set_service_sequence_for_test(&mut self, value: u64) {
        self.service_sequence = value;
    }
}

fn increment(counter: &mut u64, name: &'static str) -> Result<u64, ReaderError> {
    *counter = counter
        .checked_add(1)
        .ok_or(ReaderError::CounterOverflow { counter: name })?;
    Ok(*counter)
}

fn add(counter: &mut u64, value: u64, name: &'static str) -> Result<(), ReaderError> {
    *counter = counter
        .checked_add(value)
        .ok_or(ReaderError::CounterOverflow { counter: name })?;
    Ok(())
}

#[cfg(test)]
mod tests;
