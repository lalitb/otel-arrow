// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Dedicated blocking read/checkpoint worker.
//!
//! The worker is the sole owner of source progress, framers, Arrow builders,
//! the checkpoint store, and the one retained logical batch. The async
//! receiver owns only control, completion correlation, retry timing, and
//! downstream sends.

use std::collections::{HashMap, TryReserveError};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, SystemTimeError, UNIX_EPOCH};

use otap_df_pdata::otap::OtapArrowRecords;
use otap_df_telemetry::otel_warn;
use thiserror::Error;
use tokio::sync::mpsc as tokio_mpsc;

use super::batching::{
    BatchAppendOutcome, BatchError, LogicalBatch, OpenBatch, ProgressBase, RecordInput,
    RecordNumberTable,
};
use super::checkpoint::primitives::{FileId, LifecycleState, Locator, WAL_MAX_OPS_PER_TX};
use super::checkpoint::store::error::StoreError;
use super::checkpoint::store::{CheckpointStore, StoreOptions};
use super::config::RuntimeConfig;
use super::discovery::scanner::DiscoveryPlan;
use super::discovery::source::{DiscoveryHandle, FeedbackSendError, spawn_discovery};
use super::discovery::{
    CandidateEvent, DiscoveryError, DiscoveryFeedback, DiscoveryMessage, ReconciliationBatch,
};
use super::framing::{FramedRecord, Framer, FramerError};
use super::identity::CandidateEvidence;
use super::identity::IdentityError;
use super::identity::matcher::{
    IdentityResolution, IdentitySettings, resolve_and_persist_with_admission,
};
use super::reader::{
    ReaderError, ReaderFrontier, ReaderPoll, ReaderSettings, ReaderTable, RemovalDisposition,
    TurnDisposition,
};

const WORKER_COMMAND_CHANNEL_CAPACITY: usize = 8;
pub(super) const WORKER_EVENT_CONTROL_SLOTS: usize = 4;
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);
const FULL_HANDOFF_COMMAND_POLL: Duration = Duration::from_millis(50);

/// Commands accepted by the blocking worker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WorkerCommand {
    /// Persist the retained batch selected by a matching Ack or explicit-loss
    /// terminal policy.
    Commit {
        batch_id: u64,
        attempt: u32,
        explicit_loss: bool,
    },
    /// Shallow-clone the same retained Arrow batch for a fresh attempt.
    Resend { batch_id: u64, next_attempt: u32 },
    /// Stop admission and reading, flush bounded pending records, and drain
    /// only through real downstream completion.
    Drain,
    /// Stop immediately without advancing retained unacknowledged progress.
    Shutdown,
}

/// Outbound batch handoff. Progress deltas intentionally remain in the
/// worker's retained [`LogicalBatch`].
#[derive(Clone, Debug)]
pub(super) struct WorkerBatch {
    pub(super) batch_id: u64,
    pub(super) attempt: u32,
    pub(super) records: OtapArrowRecords,
    pub(super) record_count: u32,
    pub(super) logical_bytes: u64,
}

/// Bounded worker-to-async protocol.
#[derive(Debug)]
pub(super) enum WorkerEvent {
    Batch(WorkerBatch),
    CommitResult {
        batch_id: u64,
        attempt: u32,
        explicit_loss: bool,
        result: Result<(), WorkerError>,
    },
    Drained,
    Failed(String),
    Stopped,
}

/// Handle owned by the local async receiver.
pub(super) struct WorkerHandle {
    pub(super) command_tx: SyncSender<WorkerCommand>,
    pub(super) join: JoinHandle<Result<(), WorkerError>>,
}

/// Fail-closed worker setup, source, durability, and protocol errors.
#[derive(Debug, Error)]
pub(super) enum WorkerError {
    #[error("could not spawn filelog read/checkpoint thread: {source}")]
    ThreadSpawn {
        #[source]
        source: std::io::Error,
    },
    #[error("could not allocate bounded filelog worker resource '{resource}'")]
    AllocationFailed {
        resource: &'static str,
        #[source]
        source: TryReserveError,
    },
    #[error(transparent)]
    Discovery(#[from] DiscoveryError),
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error(transparent)]
    Reader(#[from] ReaderError),
    #[error(transparent)]
    Framer(#[from] FramerError),
    #[error(transparent)]
    Batch(#[from] BatchError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("filelog worker event channel closed")]
    EventChannelClosed,
    #[error("filelog worker command channel disconnected")]
    CommandChannelDisconnected,
    #[error("filelog discovery feedback channel disconnected")]
    DiscoveryFeedbackDisconnected,
    #[error("filelog discovery thread stopped before worker shutdown")]
    DiscoveryStopped,
    #[error("filelog discovery thread panicked")]
    DiscoveryThreadPanicked,
    #[error("filelog worker received {command} while {context}")]
    UnexpectedCommand {
        command: &'static str,
        context: &'static str,
    },
    #[error("filelog worker has no durable checkpoint record for {file_id:?}")]
    MissingCheckpointRecord { file_id: FileId },
    #[error("filelog worker checkpoint record {file_id:?} is {state:?}, not active progress state")]
    InactiveCheckpointRecord {
        file_id: FileId,
        state: LifecycleState,
    },
    #[error("filelog wall clock precedes the Unix epoch: {source}")]
    WallClockBeforeEpoch {
        #[source]
        source: SystemTimeError,
    },
    #[error("filelog observed Unix-nanosecond timestamp does not fit i64")]
    ObservedTimeOutOfRange,
    #[error(
        "filelog framer for {file_id:?} expects source offset {expected}, but reader turn starts at {actual}"
    )]
    FramerOffsetMismatch {
        file_id: FileId,
        expected: u64,
        actual: u64,
    },
    #[error(
        "filelog framer for {file_id:?} consumed {consumed} bytes from a {available}-byte input slice"
    )]
    InvalidFramerConsumption {
        file_id: FileId,
        consumed: usize,
        available: usize,
    },
    #[error("filelog framer for {file_id:?} made no progress with nonempty input")]
    FramerMadeNoProgress { file_id: FileId },
    #[error("filelog batch ID overflowed")]
    BatchIdOverflow,
    #[error(
        "filelog worker command ({batch_id}, {attempt}) does not match retained batch ({retained_batch_id}, {retained_attempt})"
    )]
    RetainedBatchMismatch {
        batch_id: u64,
        attempt: u32,
        retained_batch_id: u64,
        retained_attempt: u32,
    },
    #[error(
        "filelog resend attempt {next_attempt} is not the checked successor of retained attempt {retained_attempt} for batch {batch_id}"
    )]
    InvalidResendAttempt {
        batch_id: u64,
        retained_attempt: u32,
        next_attempt: u32,
    },
    #[error(
        "filelog reader {file_id:?} observed truncation from read offset {read_offset} and committed offset {committed_offset} to size {observed_size}; Stage 12 truncation handling is not installed"
    )]
    UnsupportedTruncation {
        file_id: FileId,
        committed_offset: u64,
        read_offset: u64,
        observed_size: u64,
    },
    #[error(
        "filelog removed reader {file_id:?} has no retained descriptor; Stage 12 late-write finalization is required before this transition can be handled safely"
    )]
    RemovedWithoutDescriptor { file_id: FileId },
    #[error(
        "filelog drain frontier {end_offset} for {file_id:?} is no longer readable at source offset {source_offset}"
    )]
    DrainFrontierUnavailable {
        file_id: FileId,
        source_offset: u64,
        end_offset: u64,
    },
    #[error(
        "filelog Updated evidence for locator {locator:?} and file {file_id:?} requires Stage 12 identity/truncation handling: {reason}"
    )]
    UnsupportedUpdatedIdentity {
        locator: Locator,
        file_id: FileId,
        reason: &'static str,
    },
    #[error("filelog retained batch is missing while processing {operation}")]
    MissingRetainedBatch { operation: &'static str },
    #[error("filelog open batch is missing while processing {operation}")]
    MissingOpenBatch { operation: &'static str },
    #[error("filelog worker internal invariant failed: {reason}")]
    Inconsistent { reason: &'static str },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoopControl {
    Continue,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HandoffControl {
    Sent,
    Drain,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AppendControl {
    Continue,
    Seal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FramerBase {
    file_epoch: u32,
    committed_offset: u64,
    framing_resume: super::checkpoint::FramingResume,
}

struct ActiveFramer {
    framer: Framer,
    base: FramerBase,
}

struct RetainedBatch {
    batch_id: u64,
    attempt: u32,
    logical: LogicalBatch,
}

struct TurnFailure {
    consumed: usize,
    error: Box<WorkerError>,
}

/// Starts the sole read/checkpoint OS thread.
pub(super) fn spawn_worker(
    config: RuntimeConfig,
    event_tx: tokio_mpsc::Sender<WorkerEvent>,
) -> Result<WorkerHandle, WorkerError> {
    // Blocking file reads, identity probes, framing, WAL writes, fsync, and
    // compaction are intentionally isolated on this one fixed OS thread.
    let (command_tx, command_rx) = sync_channel(WORKER_COMMAND_CHANNEL_CAPACITY);
    let thread_name = format!("otap-filelog-{}", config.checkpoint_id);
    let join = std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || worker_thread(config, event_tx, command_rx))
        .map_err(|source| WorkerError::ThreadSpawn { source })?;
    Ok(WorkerHandle { command_tx, join })
}

#[cfg(test)]
fn spawn_worker_with_store_fault(
    config: RuntimeConfig,
    event_tx: tokio_mpsc::Sender<WorkerEvent>,
    point: super::checkpoint::store::fault::FaultPoint,
    matching_occurrences_to_skip: usize,
) -> Result<WorkerHandle, WorkerError> {
    let (command_tx, command_rx) = sync_channel(WORKER_COMMAND_CHANNEL_CAPACITY);
    let join = std::thread::Builder::new()
        .name("otap-filelog-fault-test".to_owned())
        .spawn(move || {
            let worker =
                WorkerRuntime::new_with_store_fault(config, point, matching_occurrences_to_skip);
            run_worker_thread(worker, event_tx, command_rx)
        })
        .map_err(|source| WorkerError::ThreadSpawn { source })?;
    Ok(WorkerHandle { command_tx, join })
}

fn worker_thread(
    config: RuntimeConfig,
    event_tx: tokio_mpsc::Sender<WorkerEvent>,
    command_rx: Receiver<WorkerCommand>,
) -> Result<(), WorkerError> {
    run_worker_thread(WorkerRuntime::new(config), event_tx, command_rx)
}

fn run_worker_thread(
    worker: Result<WorkerRuntime, WorkerError>,
    event_tx: tokio_mpsc::Sender<WorkerEvent>,
    command_rx: Receiver<WorkerCommand>,
) -> Result<(), WorkerError> {
    let (result, drained) = match worker {
        Ok(mut worker) => {
            let run_result = worker.run(&event_tx, &command_rx);
            let drained = worker.drain_complete;
            let shutdown_result = worker.shutdown_resources();
            let result = run_result.and(shutdown_result);
            drop(worker);
            (result, drained)
        }
        Err(error) => (Err(error), false),
    };
    if result.is_ok() && drained {
        let _ = event_tx.blocking_send(WorkerEvent::Drained);
    }
    if let Err(error) = &result {
        let _ = event_tx.blocking_send(WorkerEvent::Failed(error.to_string()));
    }
    let _ = event_tx.blocking_send(WorkerEvent::Stopped);
    result
}

struct WorkerRuntime {
    config: RuntimeConfig,
    identity_settings: IdentitySettings,
    discovery: Option<DiscoveryHandle>,
    store: CheckpointStore,
    readers: Option<ReaderTable>,
    framers: HashMap<FileId, ActiveFramer>,
    inactive_locators: HashMap<Locator, FileId>,
    open_batch: Option<OpenBatch>,
    record_numbers: RecordNumberTable,
    retained: Option<RetainedBatch>,
    checkpoint_commit_failed: bool,
    checkpoint_maintenance_failures: u32,
    maintenance_retry_pending: bool,
    next_batch_id: u64,
    drain_requested: bool,
    drain_complete: bool,
    candidate_evidence: Vec<CandidateEvidence>,
    frontier_snapshot: Vec<ReaderFrontier>,
    rewind_targets: HashMap<FileId, (u32, u64)>,
    drain_limits: HashMap<FileId, u64>,
    drain_order: Vec<FileId>,
    drain_initialized: bool,
    max_progress_updates: usize,
}

impl WorkerRuntime {
    fn new(config: RuntimeConfig) -> Result<Self, WorkerError> {
        let store = CheckpointStore::open(StoreOptions::from_runtime_config(&config))?;
        Self::with_store(config, store)
    }

    #[cfg(test)]
    fn new_with_store_fault(
        config: RuntimeConfig,
        point: super::checkpoint::store::fault::FaultPoint,
        matching_occurrences_to_skip: usize,
    ) -> Result<Self, WorkerError> {
        let store = CheckpointStore::open_with_fault_after(
            StoreOptions::from_runtime_config(&config),
            point,
            matching_occurrences_to_skip,
        )?;
        Self::with_store(config, store)
    }

    fn with_store(config: RuntimeConfig, store: CheckpointStore) -> Result<Self, WorkerError> {
        let max_readers = usize::try_from(config.limits.max_tracked_files).map_err(|_| {
            WorkerError::Inconsistent {
                reason: "validated max_tracked_files does not fit usize",
            }
        })?;
        let max_candidate_events = usize::try_from(config.limits.max_open_files).map_err(|_| {
            WorkerError::Inconsistent {
                reason: "validated max_open_files does not fit usize",
            }
        })?;
        let max_progress_updates = usize::try_from(config.batch.max_records)
            .map_err(|_| WorkerError::Inconsistent {
                reason: "validated batch.max_records does not fit usize",
            })?
            .min(WAL_MAX_OPS_PER_TX as usize)
            .min(max_readers);

        let mut framers = HashMap::new();
        framers
            .try_reserve(max_readers)
            .map_err(|source| WorkerError::AllocationFailed {
                resource: "framer table",
                source,
            })?;
        let mut inactive_locators = HashMap::new();
        inactive_locators
            .try_reserve(max_readers)
            .map_err(|source| WorkerError::AllocationFailed {
                resource: "inactive locator table",
                source,
            })?;
        let candidate_evidence = reserved_vec(max_candidate_events, "candidate evidence batch")?;
        let frontier_snapshot = reserved_vec(max_readers, "reader frontier snapshot")?;
        let drain_order = reserved_vec(max_readers, "deterministic drain order")?;
        let mut rewind_targets = HashMap::new();
        rewind_targets
            .try_reserve(max_progress_updates)
            .map_err(|source| WorkerError::AllocationFailed {
                resource: "batch rewind target index",
                source,
            })?;
        let mut drain_limits = HashMap::new();
        drain_limits
            .try_reserve(max_readers)
            .map_err(|source| WorkerError::AllocationFailed {
                resource: "drain frontier table",
                source,
            })?;

        let identity_settings = IdentitySettings::from_runtime(&config);
        let discovery_plan = DiscoveryPlan::from_runtime(&config)?;
        let readers = ReaderTable::new(ReaderSettings::from_runtime(&config))?;
        let open_batch = OpenBatch::new(&config)?;
        let record_numbers = RecordNumberTable::new(max_readers)?;
        let discovery = spawn_discovery(discovery_plan)?;

        Ok(Self {
            config,
            identity_settings,
            discovery: Some(discovery),
            store,
            readers: Some(readers),
            framers,
            inactive_locators,
            open_batch: Some(open_batch),
            record_numbers,
            retained: None,
            checkpoint_commit_failed: false,
            checkpoint_maintenance_failures: 0,
            maintenance_retry_pending: false,
            next_batch_id: 1,
            drain_requested: false,
            drain_complete: false,
            candidate_evidence,
            frontier_snapshot,
            rewind_targets,
            drain_limits,
            drain_order,
            drain_initialized: false,
            max_progress_updates,
        })
    }

    fn run(
        &mut self,
        event_tx: &tokio_mpsc::Sender<WorkerEvent>,
        command_rx: &Receiver<WorkerCommand>,
    ) -> Result<(), WorkerError> {
        loop {
            match command_rx.try_recv() {
                Ok(command) => {
                    if self.handle_command(command, event_tx, command_rx)? == LoopControl::Shutdown
                    {
                        return Ok(());
                    }
                    continue;
                }
                Err(TryRecvError::Disconnected) => return Ok(()),
                Err(TryRecvError::Empty) => {}
            }

            if self.drain_complete {
                self.maintain_store()?;
                let wait = if self.maintenance_retry_pending {
                    COMMAND_POLL_INTERVAL
                } else {
                    self.next_wait(None)
                };
                match command_rx.recv_timeout(wait) {
                    Ok(command) => {
                        if self.handle_command(command, event_tx, command_rx)?
                            == LoopControl::Shutdown
                        {
                            return Ok(());
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                }
                continue;
            }

            if self.retained.is_some() {
                if !self.checkpoint_commit_failed {
                    self.maintain_store()?;
                }
                let wait = if self.checkpoint_commit_failed || self.maintenance_retry_pending {
                    COMMAND_POLL_INTERVAL
                } else {
                    self.next_wait(None)
                };
                match command_rx.recv_timeout(wait) {
                    Ok(command) => {
                        if self.handle_command(command, event_tx, command_rx)?
                            == LoopControl::Shutdown
                        {
                            return Ok(());
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                }
                continue;
            }

            if self.drain_requested {
                if self.drive_drain(event_tx, command_rx)? == LoopControl::Shutdown {
                    return Ok(());
                }
                continue;
            }

            self.maintain_store()?;
            if self.maintenance_retry_pending {
                match command_rx.recv_timeout(COMMAND_POLL_INTERVAL) {
                    Ok(command) => {
                        if self.handle_command(command, event_tx, command_rx)?
                            == LoopControl::Shutdown
                        {
                            return Ok(());
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                }
                continue;
            }

            if self.process_discovery_message(command_rx)? == LoopControl::Shutdown {
                return Ok(());
            }
            if self.drain_requested {
                continue;
            }

            let now = Instant::now();
            if self
                .open_batch
                .as_ref()
                .ok_or(WorkerError::MissingOpenBatch {
                    operation: "checking its flush deadline",
                })?
                .is_flush_due(now)
            {
                if self.seal_open_batch(event_tx, command_rx)? == LoopControl::Shutdown {
                    return Ok(());
                }
                continue;
            }
            if let Some(control) = self.poll_due_framer(now, event_tx, command_rx)? {
                if control == LoopControl::Shutdown {
                    return Ok(());
                }
                continue;
            }

            let poll = self.readers_mut()?.poll(now)?;
            let next_reader_probe = match poll {
                ReaderPoll::Data(turn) => {
                    if self.process_turn(turn, now, event_tx, command_rx)? == LoopControl::Shutdown
                    {
                        return Ok(());
                    }
                    continue;
                }
                ReaderPoll::EndOfFile {
                    file_id,
                    file_epoch,
                    source_offset,
                    next_probe,
                } => {
                    if self.process_eof(
                        file_id,
                        file_epoch,
                        source_offset,
                        now,
                        event_tx,
                        command_rx,
                    )? == LoopControl::Shutdown
                    {
                        return Ok(());
                    }
                    Some(next_probe)
                }
                ReaderPoll::Truncated {
                    file_id,
                    file_epoch: _,
                    committed_offset,
                    read_offset,
                    observed_size,
                } => {
                    return Err(WorkerError::UnsupportedTruncation {
                        file_id,
                        committed_offset,
                        read_offset,
                        observed_size,
                    });
                }
                ReaderPoll::EvictionRequired(request) => {
                    if self.open_batch_record_count()? != 0 {
                        self.readers_mut()?.defer_eviction(request)?;
                        if self.seal_open_batch(event_tx, command_rx)? == LoopControl::Shutdown {
                            return Ok(());
                        }
                    } else {
                        let _ = self.framers.remove(&request.victim_file_id);
                        self.readers_mut()?.confirm_eviction(request)?;
                    }
                    continue;
                }
                ReaderPoll::DescriptorCapacityBlocked { .. } => None,
                ReaderPoll::RemovedWithoutDescriptor { file_id } => {
                    return Err(WorkerError::RemovedWithoutDescriptor { file_id });
                }
                ReaderPoll::Idle { next_probe } => next_probe,
            };

            match command_rx.recv_timeout(self.next_wait(next_reader_probe)) {
                Ok(command) => {
                    if self.handle_command(command, event_tx, command_rx)? == LoopControl::Shutdown
                    {
                        return Ok(());
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    }

    fn handle_command(
        &mut self,
        command: WorkerCommand,
        event_tx: &tokio_mpsc::Sender<WorkerEvent>,
        command_rx: &Receiver<WorkerCommand>,
    ) -> Result<LoopControl, WorkerError> {
        match command {
            WorkerCommand::Shutdown => Ok(LoopControl::Shutdown),
            WorkerCommand::Drain => {
                self.drain_requested = true;
                Ok(LoopControl::Continue)
            }
            WorkerCommand::Commit {
                batch_id,
                attempt,
                explicit_loss,
            } => {
                let result = self.commit_retained(batch_id, attempt);
                self.checkpoint_commit_failed = result.is_err();
                if result.is_ok() {
                    self.checkpoint_maintenance_failures = 0;
                    self.maintenance_retry_pending = false;
                }
                let event = WorkerEvent::CommitResult {
                    batch_id,
                    attempt,
                    explicit_loss,
                    result,
                };
                let control = send_event_interruptibly(event_tx, command_rx, event)?;
                if control == HandoffControl::Shutdown {
                    return Ok(LoopControl::Shutdown);
                }
                if control == HandoffControl::Drain {
                    self.drain_requested = true;
                }
                if self.retained.is_none() && self.drain_requested {
                    return self.drive_drain(event_tx, command_rx);
                }
                Ok(LoopControl::Continue)
            }
            WorkerCommand::Resend {
                batch_id,
                next_attempt,
            } => {
                let retained = self
                    .retained
                    .as_mut()
                    .ok_or(WorkerError::MissingRetainedBatch {
                        operation: "resend",
                    })?;
                if retained.batch_id != batch_id {
                    return Err(WorkerError::RetainedBatchMismatch {
                        batch_id,
                        attempt: next_attempt,
                        retained_batch_id: retained.batch_id,
                        retained_attempt: retained.attempt,
                    });
                }
                let expected =
                    retained
                        .attempt
                        .checked_add(1)
                        .ok_or(WorkerError::InvalidResendAttempt {
                            batch_id,
                            retained_attempt: retained.attempt,
                            next_attempt,
                        })?;
                if next_attempt != expected {
                    return Err(WorkerError::InvalidResendAttempt {
                        batch_id,
                        retained_attempt: retained.attempt,
                        next_attempt,
                    });
                }
                retained.attempt = next_attempt;
                let event = WorkerEvent::Batch(worker_batch(retained));
                let control = send_event_interruptibly(event_tx, command_rx, event)?;
                match control {
                    HandoffControl::Sent => Ok(LoopControl::Continue),
                    HandoffControl::Drain => {
                        self.drain_requested = true;
                        Ok(LoopControl::Continue)
                    }
                    HandoffControl::Shutdown => Ok(LoopControl::Shutdown),
                }
            }
        }
    }

    fn process_discovery_message(
        &mut self,
        command_rx: &Receiver<WorkerCommand>,
    ) -> Result<LoopControl, WorkerError> {
        let message = match self
            .discovery
            .as_ref()
            .ok_or(WorkerError::Inconsistent {
                reason: "discovery handle is missing",
            })?
            .try_recv()
        {
            Ok(message) => message,
            Err(TryRecvError::Empty) => return Ok(LoopControl::Continue),
            Err(TryRecvError::Disconnected) => {
                return Err(WorkerError::Discovery(
                    DiscoveryError::ChannelDisconnected { channel: "event" },
                ));
            }
        };
        match message {
            DiscoveryMessage::Batch(batch) => self.process_reconciliation(*batch, command_rx),
            DiscoveryMessage::Failed(error) => Err(WorkerError::Discovery(error)),
            DiscoveryMessage::Stopped => Err(WorkerError::DiscoveryStopped),
        }
    }

    fn process_reconciliation(
        &mut self,
        batch: ReconciliationBatch,
        command_rx: &Receiver<WorkerCommand>,
    ) -> Result<LoopControl, WorkerError> {
        self.preflight_updated_candidates(&batch.events)?;
        self.candidate_evidence.clear();
        let candidate_count = batch
            .events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    CandidateEvent::Observed(_) | CandidateEvent::Updated(_)
                )
            })
            .count();
        if candidate_count > self.candidate_evidence.capacity() {
            return Err(WorkerError::Inconsistent {
                reason: "discovery candidate batch exceeds its preallocated bound",
            });
        }
        for event in &batch.events {
            match event {
                CandidateEvent::Observed(candidate) | CandidateEvent::Updated(candidate) => {
                    self.candidate_evidence.push(candidate.evidence.clone());
                }
                CandidateEvent::Removed { .. } => {}
            }
        }
        let now_unix_nano = unix_nanos()?.1;
        let resolved = resolve_and_persist_with_admission(
            &mut self.store,
            &self.candidate_evidence,
            &batch.inventory,
            &self.identity_settings,
            now_unix_nano,
        )?;
        if resolved.len() != self.candidate_evidence.len() {
            return Err(WorkerError::Inconsistent {
                reason: "identity resolution count differs from candidate event count",
            });
        }

        let mut feedback = feedback_with_capacity(batch.events.len())?;
        let mut resolved = resolved.into_iter();
        for event in batch.events {
            match event {
                CandidateEvent::Observed(candidate) => {
                    let resolution = next_resolution(&mut resolved)?;
                    let IdentityResolution::Resolved(identity) = resolution else {
                        feedback.deferred.push(candidate.evidence.locator);
                        continue;
                    };
                    let locator = candidate.evidence.locator;
                    if identity.lifecycle_state != LifecycleState::Active {
                        if let Some(existing) =
                            self.inactive_locators.insert(locator, identity.file_id)
                            && existing != identity.file_id
                        {
                            return Err(WorkerError::Inconsistent {
                                reason: "inactive locator changed durable identity",
                            });
                        }
                        feedback.durable.push(locator);
                        continue;
                    }
                    match self.readers_mut()?.insert(candidate, identity) {
                        Ok(()) => feedback.durable.push(locator),
                        Err(ReaderError::ReaderCapacityExhausted { .. }) => {
                            feedback.deferred.push(locator);
                        }
                        Err(error) => return Err(WorkerError::Reader(error)),
                    }
                }
                CandidateEvent::Updated(candidate) => {
                    let identity = match next_resolution(&mut resolved)? {
                        IdentityResolution::Resolved(identity) => identity,
                        IdentityResolution::Deferred => {
                            return Err(WorkerError::Inconsistent {
                                reason: "an Updated identity was deferred as a new registration",
                            });
                        }
                    };
                    let locator = candidate.evidence.locator;
                    if identity.lifecycle_state == LifecycleState::Active {
                        self.readers_mut()?.update(candidate, &identity)?;
                    } else if let Some(existing) =
                        self.inactive_locators.insert(locator, identity.file_id)
                        && existing != identity.file_id
                    {
                        return Err(WorkerError::Inconsistent {
                            reason: "inactive locator update changed durable identity",
                        });
                    }
                    feedback.durable.push(locator);
                }
                CandidateEvent::Removed { locator } => {
                    match self.readers_ref()?.file_id_for_locator(locator) {
                        Ok(file_id) => match self.readers_mut()?.mark_removed(locator)? {
                            RemovalDisposition::HandleRetained => {}
                            RemovalDisposition::DescriptorAbsent => {
                                return Err(WorkerError::RemovedWithoutDescriptor { file_id });
                            }
                        },
                        Err(ReaderError::UnknownLocator { .. }) => {
                            if self.inactive_locators.remove(&locator).is_none() {
                                return Err(WorkerError::Inconsistent {
                                    reason: "removed locator belongs to neither a reader nor inactive state",
                                });
                            }
                            feedback.finalized.push(locator);
                        }
                        Err(error) => return Err(WorkerError::Reader(error)),
                    }
                }
            }
        }
        if resolved.next().is_some() {
            return Err(WorkerError::Inconsistent {
                reason: "identity resolution returned unused candidates",
            });
        }
        self.send_feedback_interruptibly(feedback, command_rx)
    }

    fn preflight_updated_candidates(&self, events: &[CandidateEvent]) -> Result<(), WorkerError> {
        for event in events {
            let CandidateEvent::Updated(candidate) = event else {
                continue;
            };
            let locator = candidate.evidence.locator;
            let (file_id, committed_offset, durable_fingerprint, quarantined) = match self
                .readers_ref()?
                .identity_context(locator)
            {
                Ok(context) => (
                    context.file_id,
                    context.committed_offset,
                    context.durable_fingerprint,
                    false,
                ),
                Err(ReaderError::UnknownLocator { .. }) => {
                    let file_id = *self.inactive_locators.get(&locator).ok_or(
                            WorkerError::Inconsistent {
                                reason: "Updated locator belongs to neither a reader nor inactive state",
                            },
                        )?;
                    let record = self
                        .store
                        .table()
                        .get(&file_id)
                        .ok_or(WorkerError::MissingCheckpointRecord { file_id })?;
                    (
                        file_id,
                        record.committed_offset,
                        record.fingerprint.as_slice(),
                        record.lifecycle_state == LifecycleState::Quarantined,
                    )
                }
                Err(error) => return Err(WorkerError::Reader(error)),
            };

            let matching_records = self
                .store
                .table()
                .iter()
                .filter(|(_, record)| record.locator == locator)
                .count();
            if matching_records != 1 {
                return Err(WorkerError::UnsupportedUpdatedIdentity {
                    locator,
                    file_id,
                    reason: "runtime locator does not have exactly one durable record",
                });
            }
            if self
                .store
                .table()
                .get(&file_id)
                .is_none_or(|record| record.locator != locator)
            {
                return Err(WorkerError::UnsupportedUpdatedIdentity {
                    locator,
                    file_id,
                    reason: "reader association does not match the durable locator record",
                });
            }
            if quarantined {
                continue;
            }
            if candidate.evidence.size < committed_offset {
                return Err(WorkerError::UnsupportedUpdatedIdentity {
                    locator,
                    file_id,
                    reason: "observed size is below durable committed progress",
                });
            }
            if !candidate
                .evidence
                .fingerprint
                .starts_with(durable_fingerprint)
            {
                return Err(WorkerError::UnsupportedUpdatedIdentity {
                    locator,
                    file_id,
                    reason: "fingerprint no longer extends durable evidence",
                });
            }
        }
        Ok(())
    }

    fn send_feedback_interruptibly(
        &mut self,
        mut feedback: DiscoveryFeedback,
        command_rx: &Receiver<WorkerCommand>,
    ) -> Result<LoopControl, WorkerError> {
        loop {
            let discovery = self.discovery.as_ref().ok_or(WorkerError::Inconsistent {
                reason: "discovery handle is missing",
            })?;
            match discovery.send_feedback(feedback) {
                Ok(()) => return Ok(LoopControl::Continue),
                Err(FeedbackSendError::Disconnected(_)) => {
                    return Err(WorkerError::DiscoveryFeedbackDisconnected);
                }
                Err(FeedbackSendError::Full(returned)) => {
                    feedback = returned;
                    match command_rx.recv_timeout(FULL_HANDOFF_COMMAND_POLL) {
                        Ok(WorkerCommand::Drain) => self.drain_requested = true,
                        Ok(WorkerCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                            return Ok(LoopControl::Shutdown);
                        }
                        Ok(WorkerCommand::Commit { .. }) => {
                            return Err(WorkerError::UnexpectedCommand {
                                command: "Commit",
                                context: "retrying discovery feedback",
                            });
                        }
                        Ok(WorkerCommand::Resend { .. }) => {
                            return Err(WorkerError::UnexpectedCommand {
                                command: "Resend",
                                context: "retrying discovery feedback",
                            });
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                }
            }
        }
    }

    fn process_turn(
        &mut self,
        turn: super::reader::ReadTurn,
        now: Instant,
        event_tx: &tokio_mpsc::Sender<WorkerEvent>,
        command_rx: &Receiver<WorkerCommand>,
    ) -> Result<LoopControl, WorkerError> {
        let mut ready_clock = Instant::now;
        self.process_turn_with_clock(turn, now, event_tx, command_rx, &mut ready_clock)
    }

    fn process_turn_with_clock(
        &mut self,
        turn: super::reader::ReadTurn,
        now: Instant,
        event_tx: &tokio_mpsc::Sender<WorkerEvent>,
        command_rx: &Receiver<WorkerCommand>,
        ready_clock: &mut impl FnMut() -> Instant,
    ) -> Result<LoopControl, WorkerError> {
        let file_id = turn.file_id();
        let base = FramerBase {
            file_epoch: turn.file_epoch(),
            committed_offset: turn.committed_offset(),
            framing_resume: turn.framing_resume(),
        };
        let mut active = match self.framers.remove(&file_id) {
            Some(active) => {
                if active.base != base {
                    let error = WorkerError::Inconsistent {
                        reason: "reader turn durable base changed while a framer was active",
                    };
                    self.readers_mut()?
                        .complete_turn(turn, 0, TurnDisposition::Paused)?;
                    return Err(error);
                }
                active
            }
            None => {
                let framer = match Framer::from_runtime(
                    file_id,
                    base.file_epoch,
                    &self.config,
                    base.committed_offset,
                    base.framing_resume,
                    base.committed_offset == 0,
                    now,
                ) {
                    Ok(framer) => framer,
                    Err(error) => {
                        self.readers_mut()?
                            .complete_turn(turn, 0, TurnDisposition::Paused)?;
                        return Err(WorkerError::Framer(error));
                    }
                };
                ActiveFramer { framer, base }
            }
        };
        let expected = active.framer.next_expected_input_offset();
        if expected != turn.source_offset() {
            let actual = turn.source_offset();
            self.readers_mut()?
                .complete_turn(turn, 0, TurnDisposition::Paused)?;
            return Err(WorkerError::FramerOffsetMismatch {
                file_id,
                expected,
                actual,
            });
        }

        let outcome = self.drive_turn(&turn, &mut active, ready_clock);
        match outcome {
            Ok((consumed, AppendControl::Continue)) => {
                self.readers_mut()?
                    .complete_turn(turn, consumed, TurnDisposition::Ready)?;
                if self.framers.insert(file_id, active).is_some() {
                    return Err(WorkerError::Inconsistent {
                        reason: "framer reappeared while one turn owned it",
                    });
                }
                Ok(LoopControl::Continue)
            }
            Ok((consumed, AppendControl::Seal)) => {
                self.readers_mut()?
                    .complete_turn(turn, consumed, TurnDisposition::Paused)?;
                self.seal_open_batch(event_tx, command_rx)
            }
            Err(failure) => {
                self.readers_mut()?.complete_turn(
                    turn,
                    failure.consumed,
                    TurnDisposition::Paused,
                )?;
                Err(*failure.error)
            }
        }
    }

    fn drive_turn(
        &mut self,
        turn: &super::reader::ReadTurn,
        active: &mut ActiveFramer,
        ready_clock: &mut impl FnMut() -> Instant,
    ) -> Result<(usize, AppendControl), TurnFailure> {
        let file_id = turn.file_id();
        let mut consumed = 0usize;
        loop {
            let input = &turn.bytes()[consumed..];
            let ready_at = ready_clock();
            let step = active
                .framer
                .step(input, ready_at)
                .map_err(|error| TurnFailure {
                    consumed,
                    error: Box::new(WorkerError::Framer(error)),
                })?;
            if step.consumed > input.len() {
                return Err(TurnFailure {
                    consumed,
                    error: Box::new(WorkerError::InvalidFramerConsumption {
                        file_id,
                        consumed: step.consumed,
                        available: input.len(),
                    }),
                });
            }
            consumed += step.consumed;
            let produced = step.output.is_some();
            if let Some(record) = step.output {
                match self
                    .append_record(file_id, active.base, record, ready_at)
                    .map_err(|error| TurnFailure {
                        consumed,
                        error: Box::new(error),
                    })? {
                    AppendControl::Continue => {}
                    AppendControl::Seal => return Ok((consumed, AppendControl::Seal)),
                }
            }
            if consumed < turn.bytes().len() {
                if step.consumed == 0 && !produced {
                    return Err(TurnFailure {
                        consumed,
                        error: Box::new(WorkerError::FramerMadeNoProgress { file_id }),
                    });
                }
                continue;
            }
            if step.consumed == 0 && !produced {
                return Ok((consumed, AppendControl::Continue));
            }
            // Empty-input calls drain retained decoder/framer work one output
            // at a time after every supplied source byte was consumed.
        }
    }

    fn process_eof(
        &mut self,
        file_id: FileId,
        file_epoch: u32,
        source_offset: u64,
        now: Instant,
        event_tx: &tokio_mpsc::Sender<WorkerEvent>,
        command_rx: &Receiver<WorkerCommand>,
    ) -> Result<LoopControl, WorkerError> {
        let Some(mut active) = self.framers.remove(&file_id) else {
            return Ok(LoopControl::Continue);
        };
        if active.base.file_epoch != file_epoch
            || active.framer.next_expected_input_offset() != source_offset
        {
            return Err(WorkerError::Inconsistent {
                reason: "EOF state does not match its active framer",
            });
        }
        active.framer.observe_eof(now)?;
        loop {
            let ready_at = Instant::now();
            let step = active.framer.poll_timeout(ready_at)?;
            let produced = step.output.is_some();
            if let Some(record) = step.output
                && self.append_record(file_id, active.base, record, ready_at)?
                    == AppendControl::Seal
            {
                return self.seal_open_batch(event_tx, command_rx);
            }
            if !produced {
                break;
            }
        }
        if self.framers.insert(file_id, active).is_some() {
            return Err(WorkerError::Inconsistent {
                reason: "EOF framer reappeared while being polled",
            });
        }
        Ok(LoopControl::Continue)
    }

    fn poll_due_framer(
        &mut self,
        now: Instant,
        event_tx: &tokio_mpsc::Sender<WorkerEvent>,
        command_rx: &Receiver<WorkerCommand>,
    ) -> Result<Option<LoopControl>, WorkerError> {
        let due = self
            .framers
            .iter()
            .filter_map(|(file_id, active)| {
                active
                    .framer
                    .deadline()
                    .filter(|deadline| *deadline <= now)
                    .map(|deadline| (deadline, *file_id))
            })
            .min();
        let Some((_, file_id)) = due else {
            return Ok(None);
        };
        let mut active = self
            .framers
            .remove(&file_id)
            .ok_or(WorkerError::Inconsistent {
                reason: "due framer disappeared",
            })?;
        loop {
            let ready_at = Instant::now();
            let step = active.framer.poll_timeout(ready_at)?;
            let produced = step.output.is_some();
            if let Some(record) = step.output
                && self.append_record(file_id, active.base, record, ready_at)?
                    == AppendControl::Seal
            {
                return self.seal_open_batch(event_tx, command_rx).map(Some);
            }
            if !produced {
                break;
            }
        }
        if self.framers.insert(file_id, active).is_some() {
            return Err(WorkerError::Inconsistent {
                reason: "due framer reappeared while being polled",
            });
        }
        Ok(Some(LoopControl::Continue))
    }

    fn append_record(
        &mut self,
        file_id: FileId,
        base: FramerBase,
        framed: FramedRecord,
        ready_at: Instant,
    ) -> Result<AppendControl, WorkerError> {
        let current = current_progress(&self.store, file_id)?;
        if current.file_epoch != base.file_epoch
            || current.committed_offset != base.committed_offset
            || current.framing_resume != base.framing_resume
        {
            return Err(WorkerError::Inconsistent {
                reason: "active framer base differs from current durable progress",
            });
        }
        let (observed_time_unix_nano, last_seen_time_unix_nano) = unix_nanos()?;
        let (matched_path, resolved_path) = {
            let context = self.readers_ref()?.record_context(file_id)?;
            (
                context.matched_path.to_path_buf(),
                context.resolved_path.to_path_buf(),
            )
        };
        let fragment_index = framed.fragment.as_ref().map(|fragment| fragment.index);
        let reservation = if self.config.metadata.include_file_record_number {
            Some(
                self.record_numbers
                    .prepare(file_id, base.file_epoch, fragment_index)?,
            )
        } else {
            None
        };
        let record_number = reservation
            .as_ref()
            .and_then(super::batching::RecordNumberReservation::record_number);
        let input = RecordInput {
            framed,
            file_id,
            progress_base: current,
            matched_path,
            resolved_path,
            observed_time_unix_nano,
            last_seen_time_unix_nano,
            ready_at,
            record_number,
        };
        let outcome = self
            .open_batch
            .as_mut()
            .ok_or(WorkerError::MissingOpenBatch {
                operation: "appending one framed record",
            })?
            .try_append(input)?;
        match outcome {
            BatchAppendOutcome::Appended { seal } => {
                if let Some(reservation) = reservation {
                    let committed = self.record_numbers.commit(reservation)?;
                    if committed != record_number {
                        return Err(WorkerError::Inconsistent {
                            reason: "record-number commit changed the prepared projection",
                        });
                    }
                }
                Ok(if seal.is_some() {
                    AppendControl::Seal
                } else {
                    AppendControl::Continue
                })
            }
            BatchAppendOutcome::SealBefore {
                record: _,
                reason: _,
            } => {
                // The refused record and its uncommitted number reservation
                // are deliberately discarded. Seal rewind causes its bytes
                // to be reread after Ack.
                Ok(AppendControl::Seal)
            }
        }
    }

    fn seal_open_batch(
        &mut self,
        event_tx: &tokio_mpsc::Sender<WorkerEvent>,
        command_rx: &Receiver<WorkerCommand>,
    ) -> Result<LoopControl, WorkerError> {
        if self.retained.is_some() {
            return Err(WorkerError::Inconsistent {
                reason: "attempted to seal while a batch was already retained",
            });
        }
        let open = self
            .open_batch
            .take()
            .ok_or(WorkerError::MissingOpenBatch {
                operation: "sealing",
            })?;
        let logical = open.finish()?;
        self.open_batch = Some(OpenBatch::new(&self.config)?);

        // Every framer may contain speculative decoder lookahead, including
        // files that contributed no record to this batch.
        self.framers.clear();
        self.rewind_targets.clear();
        for delta in logical.deltas() {
            if self
                .rewind_targets
                .insert(
                    delta.file_id(),
                    (delta.expected_file_epoch(), delta.final_offset()),
                )
                .is_some()
            {
                return Err(WorkerError::Inconsistent {
                    reason: "logical batch contains duplicate progress deltas",
                });
            }
        }
        self.frontier_snapshot.clear();
        {
            let readers = self.readers.as_ref().ok_or(WorkerError::Inconsistent {
                reason: "reader table is missing",
            })?;
            self.frontier_snapshot.extend(readers.frontiers());
        }
        for index in 0..self.frontier_snapshot.len() {
            let frontier = self.frontier_snapshot[index];
            let target = match self.rewind_targets.get(&frontier.file_id) {
                Some((epoch, offset)) => {
                    if *epoch != frontier.file_epoch {
                        return Err(WorkerError::Inconsistent {
                            reason: "batch delta epoch differs from reader frontier",
                        });
                    }
                    *offset
                }
                None => frontier.committed_offset,
            };
            self.readers_mut()?.rewind_provisional_frontier(
                frontier.file_id,
                frontier.file_epoch,
                target,
            )?;
        }

        let batch_id = self.next_batch_id;
        self.next_batch_id = batch_id
            .checked_add(1)
            .ok_or(WorkerError::BatchIdOverflow)?;
        let retained = RetainedBatch {
            batch_id,
            attempt: 1,
            logical,
        };
        let event = WorkerEvent::Batch(worker_batch(&retained));
        self.retained = Some(retained);
        match send_event_interruptibly(event_tx, command_rx, event)? {
            HandoffControl::Sent => Ok(LoopControl::Continue),
            HandoffControl::Drain => {
                self.drain_requested = true;
                Ok(LoopControl::Continue)
            }
            HandoffControl::Shutdown => Ok(LoopControl::Shutdown),
        }
    }

    fn commit_retained(&mut self, batch_id: u64, attempt: u32) -> Result<(), WorkerError> {
        let retained = self
            .retained
            .take()
            .ok_or(WorkerError::MissingRetainedBatch {
                operation: "commit",
            })?;
        if retained.batch_id != batch_id || retained.attempt != attempt {
            let error = WorkerError::RetainedBatchMismatch {
                batch_id,
                attempt,
                retained_batch_id: retained.batch_id,
                retained_attempt: retained.attempt,
            };
            self.retained = Some(retained);
            return Err(error);
        }

        let result = self.commit_logical_batch(&retained.logical);
        if result.is_err() {
            self.retained = Some(retained);
        }
        result
    }

    fn commit_logical_batch(&mut self, logical: &LogicalBatch) -> Result<(), WorkerError> {
        let mut updates = reserved_vec(self.max_progress_updates, "checkpoint progress updates")?;
        for delta in logical.deltas() {
            updates
                .push(delta.to_update_progress(current_progress(&self.store, delta.file_id())?)?);
        }
        for delta in logical.deltas() {
            self.readers_mut()?.preflight_committed_progress(
                delta.file_id(),
                delta.expected_file_epoch(),
                delta.final_offset(),
            )?;
        }
        self.readers_mut()?.preflight_batch_commit()?;
        let _outcomes = self.store.commit_progress(updates)?;

        for delta in logical.deltas() {
            self.readers_mut()?.apply_preflighted_committed_progress(
                delta.file_id(),
                delta.expected_file_epoch(),
                delta.final_offset(),
                delta.final_framing_resume(),
            );
        }
        let resume = !self.drain_requested;
        self.readers_mut()?.finish_preflighted_batch_commit(resume);
        Ok(())
    }

    fn drive_drain(
        &mut self,
        event_tx: &tokio_mpsc::Sender<WorkerEvent>,
        command_rx: &Receiver<WorkerCommand>,
    ) -> Result<LoopControl, WorkerError> {
        if self.retained.is_some() {
            return Ok(LoopControl::Continue);
        }
        if !self.drain_initialized {
            self.initialize_drain_replay()?;
        }

        while let Some(file_id) = self.drain_order.last().copied() {
            let end_offset = *self
                .drain_limits
                .get(&file_id)
                .ok_or(WorkerError::Inconsistent {
                    reason: "drain order has no captured frontier",
                })?;
            let frontier = self.readers_ref()?.frontier(file_id)?;
            if frontier.read_offset < end_offset {
                let poll = self
                    .readers_mut()?
                    .poll_until(Instant::now(), file_id, end_offset)?;
                match poll {
                    ReaderPoll::Data(turn) => {
                        let control =
                            self.process_turn(turn, Instant::now(), event_tx, command_rx)?;
                        if control == LoopControl::Shutdown || self.retained.is_some() {
                            return Ok(control);
                        }
                        continue;
                    }
                    ReaderPoll::Truncated {
                        committed_offset,
                        read_offset,
                        observed_size,
                        ..
                    } => {
                        return Err(WorkerError::UnsupportedTruncation {
                            file_id,
                            committed_offset,
                            read_offset,
                            observed_size,
                        });
                    }
                    ReaderPoll::EndOfFile { source_offset, .. } => {
                        return Err(WorkerError::DrainFrontierUnavailable {
                            file_id,
                            source_offset,
                            end_offset,
                        });
                    }
                    ReaderPoll::EvictionRequired(request) => {
                        let _ = self.framers.remove(&request.victim_file_id);
                        self.readers_mut()?.confirm_eviction(request)?;
                        continue;
                    }
                    ReaderPoll::RemovedWithoutDescriptor { file_id } => {
                        return Err(WorkerError::RemovedWithoutDescriptor { file_id });
                    }
                    ReaderPoll::DescriptorCapacityBlocked { .. } | ReaderPoll::Idle { .. } => {
                        return Err(WorkerError::Inconsistent {
                            reason: "bounded drain replay could not schedule its selected reader",
                        });
                    }
                }
            }
            if frontier.read_offset > end_offset {
                return Err(WorkerError::Inconsistent {
                    reason: "drain replay advanced beyond its captured frontier",
                });
            }

            self.readers_mut()?.pause(file_id)?;
            let Some(mut active) = self.framers.remove(&file_id) else {
                let _ = self.drain_order.pop();
                let _ = self.drain_limits.remove(&file_id);
                continue;
            };
            loop {
                let ready_at = Instant::now();
                let step = active.framer.flush_drain(ready_at)?;
                let produced = step.output.is_some();
                if let Some(record) = step.output
                    && self.append_record(file_id, active.base, record, ready_at)?
                        == AppendControl::Seal
                {
                    return self.seal_open_batch(event_tx, command_rx);
                }
                if !produced {
                    // `pending` with no output is an unflushable tail under
                    // the configured disabled-partial policy. It is dropped
                    // only from memory and rewound to durable progress below.
                    break;
                }
            }
            let _ = self.drain_order.pop();
            let _ = self.drain_limits.remove(&file_id);
        }

        if self.open_batch_record_count()? != 0 {
            return self.seal_open_batch(event_tx, command_rx);
        }
        self.rewind_all_to_durable()?;
        let drain_result = self.store.drain();
        if !self.observe_checkpoint_operation(drain_result, "drain checkpoint state")? {
            return Ok(LoopControl::Continue);
        }
        self.drain_complete = true;
        Ok(LoopControl::Shutdown)
    }

    fn initialize_drain_replay(&mut self) -> Result<(), WorkerError> {
        self.frontier_snapshot.clear();
        {
            let readers = self.readers.as_ref().ok_or(WorkerError::Inconsistent {
                reason: "reader table is missing",
            })?;
            self.frontier_snapshot.extend(readers.frontiers());
        }
        self.drain_limits.clear();
        self.drain_order.clear();
        for index in 0..self.frontier_snapshot.len() {
            let frontier = self.frontier_snapshot[index];
            self.readers_mut()?.pause(frontier.file_id)?;
            if frontier.read_offset > frontier.committed_offset {
                if self
                    .drain_limits
                    .insert(frontier.file_id, frontier.read_offset)
                    .is_some()
                {
                    return Err(WorkerError::Inconsistent {
                        reason: "reader appeared twice in the drain frontier snapshot",
                    });
                }
                self.drain_order.push(frontier.file_id);
            }
        }
        self.drain_order
            .sort_unstable_by(|left, right| right.cmp(left));
        self.drain_initialized = true;
        Ok(())
    }

    fn rewind_all_to_durable(&mut self) -> Result<(), WorkerError> {
        self.frontier_snapshot.clear();
        {
            let readers = self.readers.as_ref().ok_or(WorkerError::Inconsistent {
                reason: "reader table is missing",
            })?;
            self.frontier_snapshot.extend(readers.frontiers());
        }
        for index in 0..self.frontier_snapshot.len() {
            let frontier = self.frontier_snapshot[index];
            self.readers_mut()?.rewind_provisional_frontier(
                frontier.file_id,
                frontier.file_epoch,
                frontier.committed_offset,
            )?;
        }
        Ok(())
    }

    fn maintain_store(&mut self) -> Result<(), WorkerError> {
        let result = if self.store.retired_generations().is_empty() {
            self.store
                .sync_if_due()
                .and_then(|_| self.store.compact_if_due())
                .map(|_| ())
        } else {
            self.store
                .sync_if_due()
                .and_then(|_| self.store.cleanup_retired_generations())
                .map(|_| ())
        };
        let _completed = self.observe_checkpoint_operation(result, "maintain checkpoint state")?;
        Ok(())
    }

    fn observe_checkpoint_operation(
        &mut self,
        result: Result<(), StoreError>,
        operation: &'static str,
    ) -> Result<bool, WorkerError> {
        match result {
            Ok(()) => {
                self.checkpoint_maintenance_failures = 0;
                self.maintenance_retry_pending = false;
                Ok(true)
            }
            Err(error) => {
                self.checkpoint_maintenance_failures = self
                    .checkpoint_maintenance_failures
                    .checked_add(1)
                    .ok_or(WorkerError::Inconsistent {
                        reason: "checkpoint maintenance failure counter overflowed",
                    })?;
                otel_warn!(
                    "filelog_receiver.checkpoint_operation_failed",
                    checkpoint_id = self.config.checkpoint_id.as_str(),
                    operation = operation,
                    consecutive_failures = u64::from(self.checkpoint_maintenance_failures),
                    error = error.to_string()
                );
                if self.checkpoint_maintenance_failures
                    >= self.config.checkpoint.max_consecutive_failures
                {
                    return Err(WorkerError::Store(error));
                }
                self.maintenance_retry_pending = true;
                Ok(false)
            }
        }
    }

    fn next_wait(&self, reader_deadline: Option<Instant>) -> Duration {
        let now = Instant::now();
        let mut wait = COMMAND_POLL_INTERVAL;
        for deadline in [
            reader_deadline,
            self.store.next_sync_deadline(),
            self.open_batch.as_ref().and_then(OpenBatch::deadline),
            self.framers
                .values()
                .filter_map(|active| active.framer.deadline())
                .min(),
        ]
        .into_iter()
        .flatten()
        {
            wait = wait.min(deadline.saturating_duration_since(now));
        }
        wait
    }

    fn open_batch_record_count(&self) -> Result<u32, WorkerError> {
        self.open_batch
            .as_ref()
            .map(OpenBatch::record_count)
            .ok_or(WorkerError::MissingOpenBatch {
                operation: "reading its record count",
            })
    }

    fn readers_ref(&self) -> Result<&ReaderTable, WorkerError> {
        self.readers.as_ref().ok_or(WorkerError::Inconsistent {
            reason: "reader table is missing",
        })
    }

    fn readers_mut(&mut self) -> Result<&mut ReaderTable, WorkerError> {
        self.readers.as_mut().ok_or(WorkerError::Inconsistent {
            reason: "reader table is missing",
        })
    }

    fn shutdown_resources(&mut self) -> Result<(), WorkerError> {
        let mut first_error = None;
        if let Some(discovery) = self.discovery.take() {
            discovery.request_shutdown();
            if discovery.into_join_handle().join().is_err() {
                first_error = Some(WorkerError::DiscoveryThreadPanicked);
            }
        }
        loop {
            let result = self.store.drain();
            match self.observe_checkpoint_operation(result, "shut down checkpoint state") {
                Ok(true) => break,
                Ok(false) => std::thread::yield_now(),
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                    break;
                }
            }
        }
        if let Some(readers) = self.readers.take()
            && let Err(error) = readers.shutdown()
            && first_error.is_none()
        {
            first_error = Some(WorkerError::Reader(error));
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Persists a recordless lifecycle delta without synthesizing OTAP.
    ///
    /// Stage 12 decides when finalization is valid; Stage 11 provides only
    /// this narrow, directly testable durability primitive.
    fn commit_direct_progress(
        &mut self,
        delta: &super::batching::ProgressDelta,
    ) -> Result<(), WorkerError> {
        persist_direct_progress(&mut self.store, delta)?;
        self.readers_mut()?.observe_committed_progress(
            delta.file_id(),
            delta.expected_file_epoch(),
            delta.final_offset(),
            delta.final_framing_resume(),
        )?;
        if delta.finalize() {
            let _ = self.readers_mut()?.release_finalized(delta.file_id())?;
            let _ = self.record_numbers.remove(delta.file_id());
        }
        Ok(())
    }
}

fn worker_batch(retained: &RetainedBatch) -> WorkerBatch {
    WorkerBatch {
        batch_id: retained.batch_id,
        attempt: retained.attempt,
        records: retained.logical.outbound_records(),
        record_count: retained.logical.record_count(),
        logical_bytes: retained.logical.logical_bytes(),
    }
}

fn send_event_interruptibly(
    event_tx: &tokio_mpsc::Sender<WorkerEvent>,
    command_rx: &Receiver<WorkerCommand>,
    mut event: WorkerEvent,
) -> Result<HandoffControl, WorkerError> {
    let mut drain_requested = false;
    loop {
        match event_tx.try_send(event) {
            Ok(()) => {
                return Ok(if drain_requested {
                    HandoffControl::Drain
                } else {
                    HandoffControl::Sent
                });
            }
            // Async teardown closes the event receiver before joining so the
            // worker can never deadlock on a terminal event queued ahead of
            // Shutdown. The join result still carries genuine checkpoint,
            // discovery, and lease cleanup failures.
            Err(tokio_mpsc::error::TrySendError::Closed(_)) => {
                return Ok(HandoffControl::Shutdown);
            }
            Err(tokio_mpsc::error::TrySendError::Full(returned)) => {
                event = returned;
                match command_rx.recv_timeout(FULL_HANDOFF_COMMAND_POLL) {
                    Ok(WorkerCommand::Drain) => drain_requested = true,
                    Ok(WorkerCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                        return Ok(HandoffControl::Shutdown);
                    }
                    Ok(WorkerCommand::Commit { .. }) => {
                        return Err(WorkerError::UnexpectedCommand {
                            command: "Commit",
                            context: "handing a worker event to async",
                        });
                    }
                    Ok(WorkerCommand::Resend { .. }) => {
                        return Err(WorkerError::UnexpectedCommand {
                            command: "Resend",
                            context: "handing a worker event to async",
                        });
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                }
            }
        }
    }
}

fn feedback_with_capacity(capacity: usize) -> Result<DiscoveryFeedback, WorkerError> {
    Ok(DiscoveryFeedback {
        durable: reserved_vec(capacity, "discovery durable feedback")?,
        rejected: reserved_vec(capacity, "discovery rejected feedback")?,
        deferred: reserved_vec(capacity, "discovery deferred feedback")?,
        finalized: reserved_vec(capacity, "discovery finalized feedback")?,
    })
}

fn reserved_vec<T>(capacity: usize, resource: &'static str) -> Result<Vec<T>, WorkerError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|source| WorkerError::AllocationFailed { resource, source })?;
    Ok(values)
}

fn next_resolution(
    resolved: &mut impl Iterator<Item = IdentityResolution>,
) -> Result<IdentityResolution, WorkerError> {
    resolved.next().ok_or(WorkerError::Inconsistent {
        reason: "candidate event has no identity resolution",
    })
}

fn unix_nanos() -> Result<(i64, u64), WorkerError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|source| WorkerError::WallClockBeforeEpoch { source })?;
    let nanos = duration.as_nanos();
    let last_seen_time_unix_nano =
        u64::try_from(nanos).map_err(|_| WorkerError::ObservedTimeOutOfRange)?;
    let observed_time_unix_nano =
        i64::try_from(nanos).map_err(|_| WorkerError::ObservedTimeOutOfRange)?;
    Ok((observed_time_unix_nano, last_seen_time_unix_nano))
}

fn current_progress(store: &CheckpointStore, file_id: FileId) -> Result<ProgressBase, WorkerError> {
    let record = store
        .table()
        .get(&file_id)
        .ok_or(WorkerError::MissingCheckpointRecord { file_id })?;
    if record.lifecycle_state != LifecycleState::Active {
        return Err(WorkerError::InactiveCheckpointRecord {
            file_id,
            state: record.lifecycle_state,
        });
    }
    Ok(ProgressBase {
        file_epoch: record.file_epoch,
        committed_offset: record.committed_offset,
        framing_resume: record.framing_resume,
        last_seen_time_unix_nano: record.last_seen_time_unix_nano,
    })
}

fn persist_direct_progress(
    store: &mut CheckpointStore,
    delta: &super::batching::ProgressDelta,
) -> Result<(), WorkerError> {
    let update = delta.to_update_progress(current_progress(store, delta.file_id())?)?;
    let mut updates = reserved_vec(1, "direct checkpoint progress update")?;
    updates.push(update);
    let _outcomes = store.commit_progress(updates)?;
    Ok(())
}

#[cfg(test)]
#[path = "worker/tests.rs"]
mod tests;
