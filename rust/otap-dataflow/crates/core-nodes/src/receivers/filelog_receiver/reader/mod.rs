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

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::collections::{TryReserveError, hash_map::Entry};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use thiserror::Error;

use super::checkpoint::{FileId, FramingResume, LifecycleState, Locator};
use super::config::RuntimeConfig;
use super::discovery::DiscoveredCandidate;
use super::identity::IdentityError;
use super::identity::matcher::ResolvedIdentity;
use super::identity::platform::{encode_advisory_path, read_source_at, reopen_candidate_at};
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
            eof_probe_interval: config.discovery.poll_interval,
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
    /// Inspecting an open source handle after EOF failed.
    #[error("could not inspect filelog source {path} for {file_id:?}: {source}")]
    Metadata {
        /// Durable identity being inspected.
        file_id: FileId,
        /// Advisory path for diagnostics only.
        path: PathBuf,
        /// Operating-system metadata failure.
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

/// Result of one scheduler poll.
#[derive(Debug)]
pub(crate) enum ReaderPoll {
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
    },
    /// A descriptor can rotate only after caller-owned uncommitted state is
    /// discarded.
    EvictionRequired(EvictionRequest),
    /// Every descriptor is temporarily non-evictable, for example because
    /// removed handles are pinned for late writes.
    DescriptorCapacityBlocked {
        /// Closed reader waiting for a slot.
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
    /// Readers removed from discovery but not finalized.
    pub(crate) removed_readers: usize,
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
    /// Uncommitted source bytes rewound by confirmed eviction.
    pub(crate) source_bytes_rewound: u64,
    /// Temporary EOF observations.
    pub(crate) eof_observations: u64,
}

#[derive(Debug)]
struct ActivityCounters {
    read_turns: u64,
    source_bytes_read: u64,
    opens: u64,
    reopens: u64,
    descriptor_evictions: u64,
    source_bytes_rewound: u64,
    eof_observations: u64,
}

impl ActivityCounters {
    const fn new() -> Self {
        Self {
            read_turns: 0,
            source_bytes_read: 0,
            opens: 0,
            reopens: 0,
            descriptor_evictions: 0,
            source_bytes_rewound: 0,
            eof_observations: 0,
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
    present: bool,
    ever_opened: bool,
    resident: Option<ResidentReader>,
    schedule: ScheduleState,
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
    open_count: usize,
    service_sequence: u64,
    turn_sequence: u64,
    eviction_sequence: u64,
    pending_eviction: Option<EvictionRequest>,
    read_buffer: Option<Vec<u8>>,
    counters: ActivityCounters,
}

impl ReaderTable {
    /// Creates a bounded table and reserves its one shared source-read
    /// buffer before any file can be admitted.
    pub(crate) fn new(settings: ReaderSettings) -> Result<Self, ReaderError> {
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
            open_count: 0,
            service_sequence: 0,
            turn_sequence: 0,
            eviction_sequence: 0,
            pending_eviction: None,
            read_buffer: Some(read_buffer),
            counters: ActivityCounters::new(),
        })
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
        let lease = self.lease_scope.try_acquire(candidate.evidence.locator)?;
        let file_id = resolved.file_id;
        let locator = candidate.evidence.locator;
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
            present: true,
            ever_opened: false,
            resident: None,
            schedule: ScheduleState::Ready,
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
        let file_id = *self
            .by_locator
            .get(&locator)
            .ok_or(ReaderError::UnknownLocator { locator })?;
        self.cancel_eviction_involving(file_id)?;
        let has_descriptor = {
            let reader = self
                .readers
                .get_mut(&file_id)
                .ok_or(ReaderError::Inconsistent {
                    reason: "locator index points to a missing reader",
                })?;
            reader.present = false;
            reader.resident.is_some()
        };
        if has_descriptor {
            Ok(RemovalDisposition::HandleRetained)
        } else {
            self.pause(file_id)?;
            Ok(RemovalDisposition::DescriptorAbsent)
        }
    }

    /// Promotes due EOF readers, then serves at most one ready reader.
    pub(crate) fn poll(&mut self, now: Instant) -> Result<ReaderPoll, ReaderError> {
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
        self.activate_due(now)?;
        self.promote_descriptor_waiter()?;

        let Some(file_id) = self.ready.pop_front() else {
            return Ok(ReaderPoll::Idle {
                next_probe: self.eof_deadlines.first().map(|(deadline, _)| *deadline),
            });
        };
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
                return Ok(ReaderPoll::DescriptorCapacityBlocked { file_id });
            }
            if let Err(error) = self.open_reader(file_id) {
                self.readers
                    .get_mut(&file_id)
                    .ok_or(ReaderError::Inconsistent {
                        reason: "failed reopen target disappeared",
                    })?
                    .schedule = ScheduleState::Paused;
                return Err(error);
            }
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
        buffer.resize(self.settings.max_read_bytes_per_turn, 0);
        let _read_turns = increment(&mut self.counters.read_turns, "source read turns")?;
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
            read_source_at(&resident.file, source_offset, &mut buffer)
        };
        let count = match read_result {
            Ok(count) => count,
            Err(source) => {
                self.read_buffer = Some(buffer);
                return Err(ReaderError::Read {
                    file_id,
                    path: diagnostic_path,
                    source,
                });
            }
        };

        if count == 0 {
            self.read_buffer = Some(buffer);
            let observed_size = {
                let reader = self
                    .readers
                    .get(&file_id)
                    .ok_or(ReaderError::Inconsistent {
                        reason: "EOF reader disappeared before metadata inspection",
                    })?;
                let resident = reader.resident.as_ref().ok_or(ReaderError::Inconsistent {
                    reason: "EOF reader descriptor disappeared before metadata inspection",
                })?;
                resident
                    .file
                    .metadata()
                    .map_err(|source| ReaderError::Metadata {
                        file_id,
                        path: diagnostic_path,
                        source,
                    })?
                    .len()
            };
            if observed_size < source_offset {
                return Ok(ReaderPoll::Truncated {
                    file_id,
                    file_epoch,
                    committed_offset,
                    read_offset: source_offset,
                    observed_size,
                });
            }
            let next_probe = now
                .checked_add(self.settings.eof_probe_interval)
                .ok_or(ReaderError::DeadlineOverflow)?;
            let reader = self
                .readers
                .get_mut(&file_id)
                .ok_or(ReaderError::Inconsistent {
                    reason: "EOF reader disappeared",
                })?;
            reader.schedule = ScheduleState::Eof { next_probe };
            if !self.eof_deadlines.insert((next_probe, file_id)) {
                return Err(ReaderError::Inconsistent {
                    reason: "EOF deadline was already present",
                });
            }
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
        Ok(())
    }

    /// Makes a paused or EOF reader immediately eligible for a later
    /// round-robin turn.
    pub(crate) fn make_ready(&mut self, file_id: FileId) -> Result<(), ReaderError> {
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
        self.ready.push_back(file_id);
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
            ScheduleState::Ready => self.remove_ready(file_id)?,
            ScheduleState::Eof { next_probe } => {
                if !self.eof_deadlines.remove(&(next_probe, file_id)) {
                    return Err(ReaderError::Inconsistent {
                        reason: "paused EOF reader has no deadline entry",
                    });
                }
            }
            ScheduleState::DescriptorBlocked => {}
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

        self.remove_scheduling_state(file_id)?;
        let reader = self
            .readers
            .get_mut(&file_id)
            .ok_or(ReaderError::Inconsistent {
                reason: "rewound reader disappeared",
            })?;
        debug_assert_eq!(reader.committed_offset, committed_offset);
        reader.read_offset = target_offset;
        reader.schedule = ScheduleState::Paused;
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
        let reader = self
            .readers
            .get_mut(&file_id)
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
        reader.committed_offset = committed_offset;
        reader.framing_resume = framing_resume;
        if self
            .pending_eviction
            .is_some_and(|request| request.victim_file_id == file_id)
        {
            self.pending_eviction = None;
        }
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

    /// Defers one pending eviction and pauses its target, allowing a later
    /// batch boundary or lifecycle decision to make the target ready again.
    pub(crate) fn defer_eviction(&mut self, request: EvictionRequest) -> Result<(), ReaderError> {
        if self.pending_eviction != Some(request) {
            return Err(ReaderError::InvalidTurn {
                file_id: request.target_file_id,
                ticket: request.ticket,
            });
        }
        self.pending_eviction = None;
        self.pause(request.target_file_id)
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

    /// Returns a snapshot of bounded populations and monotonic activity.
    #[must_use]
    pub(crate) fn stats(&self) -> ReaderStats {
        ReaderStats {
            tracked_readers: self.readers.len(),
            open_files: self.open_count,
            ready_readers: self.ready.len(),
            eof_readers: self.eof_deadlines.len(),
            removed_readers: self
                .readers
                .values()
                .filter(|reader| !reader.present)
                .count(),
            read_turns: self.counters.read_turns,
            source_bytes_read: self.counters.source_bytes_read,
            opens: self.counters.opens,
            reopens: self.counters.reopens,
            descriptor_evictions: self.counters.descriptor_evictions,
            source_bytes_rewound: self.counters.source_bytes_rewound,
            eof_observations: self.counters.eof_observations,
        }
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
        self.open_count = 0;
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
            self.ready.push_back(file_id);
        }
        Ok(())
    }

    fn open_reader(&mut self, file_id: FileId) -> Result<(), ReaderError> {
        let (
            resolved_path,
            matched_path,
            locator,
            durable_fingerprint,
            committed_offset,
            ever_opened,
        ) = {
            let reader = self
                .readers
                .get(&file_id)
                .ok_or(ReaderError::UnknownFile { file_id })?;
            (
                reader.resolved_path.clone(),
                reader.matched_path.clone(),
                reader.locator,
                reader.durable_fingerprint.clone(),
                reader.committed_offset,
                reader.ever_opened,
            )
        };
        let opened = reopen_candidate_at(
            &resolved_path,
            &matched_path,
            self.settings.follow_symlinks,
            self.settings.fingerprint_bytes,
            self.settings.ignored_header_bytes,
            locator,
            &durable_fingerprint,
            committed_offset,
        )
        .map_err(|source| ReaderError::Reopen { file_id, source })?;
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
        Ok(())
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
                    Ok(())
                } else {
                    Err(ReaderError::Inconsistent {
                        reason: "finalized EOF reader has no deadline entry",
                    })
                }
            }
            ScheduleState::DescriptorBlocked => Ok(()),
            ScheduleState::Paused => Ok(()),
            ScheduleState::InFlight { .. } => Err(ReaderError::InvalidState {
                file_id,
                operation: "remove scheduling state",
                state: state.name(),
            }),
        }
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
            .readers
            .values()
            .filter(|reader| {
                matches!(reader.schedule, ScheduleState::DescriptorBlocked)
                    && (self.open_count < self.settings.max_open_files
                        || self.select_lrs_victim(reader.file_id).is_some())
            })
            .map(|reader| reader.file_id)
            .min();
        if let Some(file_id) = next {
            let reader = self
                .readers
                .get_mut(&file_id)
                .ok_or(ReaderError::Inconsistent {
                    reason: "descriptor waiter disappeared",
                })?;
            reader.schedule = ScheduleState::Ready;
            self.ready.push_back(file_id);
        }
        Ok(())
    }

    fn release_reader(&mut self, file_id: FileId) -> Result<Locator, ReaderError> {
        self.remove_scheduling_state(file_id)?;
        let reader = self
            .readers
            .remove(&file_id)
            .ok_or(ReaderError::Inconsistent {
                reason: "released reader disappeared",
            })?;
        if reader.resident.is_some() {
            self.open_count = self
                .open_count
                .checked_sub(1)
                .ok_or(ReaderError::Inconsistent {
                    reason: "open descriptor count underflowed during release",
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
