// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Dedicated blocking read/checkpoint worker.
//!
//! The worker is the sole owner of source progress, framers, Arrow builders,
//! the checkpoint store, and the one retained logical batch. The async
//! receiver owns only control, completion correlation, retry timing, and
//! downstream sends.

use std::collections::{HashMap, HashSet, TryReserveError};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, SystemTimeError, UNIX_EPOCH};

use otap_df_pdata::otap::OtapArrowRecords;
use otap_df_telemetry::{otel_info, otel_warn};
use thiserror::Error;
use tokio::sync::mpsc as tokio_mpsc;

use super::batching::{
    BatchAppendOutcome, BatchError, FinalizationOutcome, LogicalBatch, OpenBatch, ProgressBase,
    ProgressFrontier, RecordInput, RecordNumberTable,
};
use super::checkpoint::primitives::{
    FileId, FramingResume, LifecycleState, Locator, QUARANTINE_REASON_DECODE,
    QUARANTINE_REASON_TRUNCATE, TRUNCATE_RESET_REASON_READ_NEW, WAL_MAX_OPS_PER_TX,
};
use super::checkpoint::store::error::StoreError;
use super::checkpoint::store::{CheckpointStore, StoreOptions, StoreStats};
use super::checkpoint::wal::{Operation, QuarantineFile, ResetAfterTruncate};
use super::config::{OnTruncate, RuntimeConfig};
use super::discovery::scanner::DiscoveryPlan;
use super::discovery::source::{
    DiscoveryHandle, FeedbackSendError, spawn_discovery_with_shutdown_signal,
};
use super::discovery::{
    CandidateEvent, DiscoveryError, DiscoveryFeedback, DiscoveryMessage, DurableAck,
    ReconciliationBatch,
};
use super::framing::{DecodeError, DecodeOutcome, FlushReason, FramedRecord, Framer, FramerError};
use super::identity::CandidateEvidence;
use super::identity::IdentityError;
use super::identity::matcher::{
    IdentityMatch, IdentityResolution, IdentitySettings, plan_with_admission,
};
use super::lease::LeaseError;
use super::reader::{
    CandidateEvidenceRefresh, ReaderError, ReaderFrontier, ReaderPoll, ReaderSettings, ReaderTable,
    RemovalDisposition, TurnDisposition,
};
use super::telemetry::{
    HealthEventCategory, HealthEventLimiter, WorkerCounter, WorkerGauge, WorkerTelemetryBridge,
    duration_ns,
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
    pub(super) source_bytes: u64,
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
    pub(super) telemetry: Arc<WorkerTelemetryBridge>,
    pub(super) shutdown_requested: Arc<AtomicBool>,
}

/// Fail-closed worker setup, source, durability, and protocol errors.
#[derive(Debug, Error)]
pub(super) enum WorkerError {
    #[error("filelog worker startup was cancelled")]
    StartupCancelled,
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
        "filelog reader {file_id:?} observed truncation from read offset {read_offset} and committed offset {committed_offset} to size {observed_size}"
    )]
    Truncation {
        file_id: FileId,
        committed_offset: u64,
        read_offset: u64,
        observed_size: u64,
    },
    #[error("filelog rotation deadline overflowed for {file_id:?}")]
    RotationDeadlineOverflow { file_id: FileId },
    #[error("filelog reconciliation retry deadline overflowed")]
    ReconciliationRetryDeadlineOverflow,
    #[error(
        "filelog drain frontier {end_offset} for {file_id:?} is no longer readable at source offset {source_offset}"
    )]
    DrainFrontierUnavailable {
        file_id: FileId,
        source_offset: u64,
        end_offset: u64,
    },
    #[error(
        "filelog Updated evidence for locator {locator:?} and file {file_id:?} is inconsistent: {reason}"
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
    SealBefore,
    SealAfter,
}

impl AppendControl {
    const fn requires_seal(self) -> bool {
        !matches!(self, Self::Continue)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FramerBase {
    file_epoch: u32,
    committed_offset: u64,
    framing_resume: FramingResume,
}

struct ActiveFramer {
    framer: Framer,
    base: FramerBase,
    pending_bytes: u64,
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

#[derive(Clone, Debug)]
struct DetectedTruncation {
    file_id: FileId,
    expected_file_epoch: u32,
    expected_committed_offset: u64,
    observed_size: u64,
    observed_fingerprint: Vec<u8>,
    locator: Locator,
    present: bool,
}

#[derive(Clone, Copy, Debug)]
struct RotationWait {
    stable_since: Instant,
    deadline: Instant,
}

#[derive(Clone, Copy, Debug)]
struct FramedTelemetry {
    decode_outcome: DecodeOutcome,
    flush_reason: Option<FlushReason>,
    truncated: bool,
    discarded_source_bytes: u64,
    split: bool,
}

/// Starts the sole read/checkpoint OS thread.
pub(super) fn spawn_worker(
    config: RuntimeConfig,
    event_tx: tokio_mpsc::Sender<WorkerEvent>,
) -> Result<WorkerHandle, WorkerError> {
    // Blocking file reads, identity probes, framing, WAL writes, fsync, and
    // compaction are intentionally isolated on this one fixed OS thread.
    let (command_tx, command_rx) = sync_channel(WORKER_COMMAND_CHANNEL_CAPACITY);
    let telemetry = Arc::new(WorkerTelemetryBridge::default());
    let worker_telemetry = Arc::clone(&telemetry);
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let worker_shutdown_requested = Arc::clone(&shutdown_requested);
    let thread_name = "otap-filelog-worker".to_owned();
    let join = std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            worker_thread(
                config,
                event_tx,
                command_rx,
                worker_telemetry,
                worker_shutdown_requested,
            )
        })
        .map_err(|source| WorkerError::ThreadSpawn { source })?;
    Ok(WorkerHandle {
        command_tx,
        join,
        telemetry,
        shutdown_requested,
    })
}

#[cfg(test)]
fn spawn_worker_with_store_fault(
    config: RuntimeConfig,
    event_tx: tokio_mpsc::Sender<WorkerEvent>,
    point: super::checkpoint::store::fault::FaultPoint,
    matching_occurrences_to_skip: usize,
) -> Result<WorkerHandle, WorkerError> {
    let (command_tx, command_rx) = sync_channel(WORKER_COMMAND_CHANNEL_CAPACITY);
    let telemetry = Arc::new(WorkerTelemetryBridge::default());
    let worker_telemetry = Arc::clone(&telemetry);
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let worker_shutdown_requested = Arc::clone(&shutdown_requested);
    let join = std::thread::Builder::new()
        .name("otap-filelog-fault-test".to_owned())
        .spawn(move || {
            let worker = WorkerRuntime::new_with_store_fault_and_telemetry(
                config,
                point,
                matching_occurrences_to_skip,
                worker_telemetry,
                worker_shutdown_requested,
            );
            run_worker_thread(worker, event_tx, command_rx)
        })
        .map_err(|source| WorkerError::ThreadSpawn { source })?;
    Ok(WorkerHandle {
        command_tx,
        join,
        telemetry,
        shutdown_requested,
    })
}

fn worker_thread(
    config: RuntimeConfig,
    event_tx: tokio_mpsc::Sender<WorkerEvent>,
    command_rx: Receiver<WorkerCommand>,
    telemetry: Arc<WorkerTelemetryBridge>,
    shutdown_requested: Arc<AtomicBool>,
) -> Result<(), WorkerError> {
    let worker =
        WorkerRuntime::new_with_telemetry(config, Arc::clone(&telemetry), shutdown_requested);
    match &worker {
        Err(WorkerError::Store(StoreError::NamespaceLocked { waited, .. })) => {
            telemetry.add(WorkerCounter::NamespaceLockFailures, 1);
            telemetry.add(WorkerCounter::NamespaceLockWaits, 1);
            telemetry.add(WorkerCounter::NamespaceLockContentions, 1);
            telemetry.add(WorkerCounter::NamespaceLockWaitNs, duration_ns(*waited));
        }
        Err(WorkerError::Store(_)) => {
            telemetry.add(WorkerCounter::CheckpointFailures, 1);
        }
        _ => {}
    }
    run_worker_thread(worker, event_tx, command_rx)
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
            worker.publish_observations();
            let result = run_result.and(shutdown_result);
            drop(worker);
            (result, drained)
        }
        Err(WorkerError::StartupCancelled) => (Ok(()), false),
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
    rotation_waits: HashMap<FileId, RotationWait>,
    pending_reconciliation: Option<ReconciliationBatch>,
    pending_reconciliation_retry_at: Option<Instant>,
    pending_finalizations: Vec<Locator>,
    detected_truncations: Vec<DetectedTruncation>,
    open_batch: Option<OpenBatch>,
    record_numbers: RecordNumberTable,
    retained: Option<RetainedBatch>,
    pending_decode_quarantine: Option<PendingDecodeQuarantine>,
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
    telemetry: Arc<WorkerTelemetryBridge>,
    health_events: HealthEventLimiter,
    observed_store: ObservedStoreStats,
    partial_bytes_pending: u64,
    shutdown_requested: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ObservedStoreStats {
    wal_bytes_appended: u64,
    transactions_appended: u64,
    syncs: u64,
    persist_duration_ns: u64,
    persist_operations: u64,
    sync_duration_ns: u64,
    sync_operations: u64,
    namespace_lock_wait_ns: u64,
    namespace_lock_contentions: u64,
    namespace_lock_observed: bool,
    quarantine_reset_beginning: u64,
    quarantine_reset_end: u64,
    quarantine_keep_failed: u64,
    quarantine_removals: u64,
}

#[derive(Clone, Copy, Debug)]
struct PendingDecodeQuarantine {
    file_id: FileId,
    observed_size: u64,
}

impl WorkerRuntime {
    fn cancellation_requested(&self) -> bool {
        self.shutdown_requested.load(Ordering::Acquire)
    }

    fn new(config: RuntimeConfig) -> Result<Self, WorkerError> {
        Self::new_with_telemetry(
            config,
            Arc::new(WorkerTelemetryBridge::default()),
            Arc::new(AtomicBool::new(false)),
        )
    }

    fn new_with_telemetry(
        config: RuntimeConfig,
        telemetry: Arc<WorkerTelemetryBridge>,
        shutdown_requested: Arc<AtomicBool>,
    ) -> Result<Self, WorkerError> {
        let cancellation = Arc::clone(&shutdown_requested);
        let store =
            CheckpointStore::open_cancellable(StoreOptions::from_runtime_config(&config), || {
                cancellation.load(Ordering::Acquire)
            })?;
        let Some(store) = store else {
            return Err(WorkerError::StartupCancelled);
        };
        Self::with_store(config, store, telemetry, shutdown_requested)
    }

    #[cfg(test)]
    fn new_with_store_fault_and_telemetry(
        config: RuntimeConfig,
        point: super::checkpoint::store::fault::FaultPoint,
        matching_occurrences_to_skip: usize,
        telemetry: Arc<WorkerTelemetryBridge>,
        shutdown_requested: Arc<AtomicBool>,
    ) -> Result<Self, WorkerError> {
        let store = CheckpointStore::open_with_fault_after(
            StoreOptions::from_runtime_config(&config),
            point,
            matching_occurrences_to_skip,
        )?;
        Self::with_store(config, store, telemetry, shutdown_requested)
    }

    fn with_store(
        config: RuntimeConfig,
        store: CheckpointStore,
        telemetry: Arc<WorkerTelemetryBridge>,
        shutdown_requested: Arc<AtomicBool>,
    ) -> Result<Self, WorkerError> {
        if shutdown_requested.load(Ordering::Acquire) {
            return Err(WorkerError::StartupCancelled);
        }
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
        let mut rotation_waits = HashMap::new();
        rotation_waits.try_reserve(max_readers).map_err(|source| {
            WorkerError::AllocationFailed {
                resource: "rotation wait table",
                source,
            }
        })?;
        let pending_finalizations = reserved_vec(max_readers, "pending discovery finalizations")?;
        let detected_truncations = reserved_vec(max_candidate_events, "detected truncation batch")?;
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

        if shutdown_requested.load(Ordering::Acquire) {
            return Err(WorkerError::StartupCancelled);
        }
        let identity_settings = IdentitySettings::from_runtime(&config);
        let discovery_plan = DiscoveryPlan::from_runtime(&config)?;
        if shutdown_requested.load(Ordering::Acquire) {
            return Err(WorkerError::StartupCancelled);
        }
        let readers = ReaderTable::new_with_shutdown_signal(
            ReaderSettings::from_runtime(&config),
            Arc::clone(&shutdown_requested),
        )?;
        let open_batch = OpenBatch::new(&config)?;
        let record_numbers = RecordNumberTable::new(max_readers)?;
        if shutdown_requested.load(Ordering::Acquire) {
            return Err(WorkerError::StartupCancelled);
        }
        let discovery =
            spawn_discovery_with_shutdown_signal(discovery_plan, Arc::clone(&shutdown_requested))?;
        if shutdown_requested.load(Ordering::Acquire) {
            drop(discovery);
            return Err(WorkerError::StartupCancelled);
        }

        let mut worker = Self {
            config,
            identity_settings,
            discovery: Some(discovery),
            store,
            readers: Some(readers),
            framers,
            inactive_locators,
            rotation_waits,
            pending_reconciliation: None,
            pending_reconciliation_retry_at: None,
            pending_finalizations,
            detected_truncations,
            open_batch: Some(open_batch),
            record_numbers,
            retained: None,
            pending_decode_quarantine: None,
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
            telemetry,
            health_events: HealthEventLimiter::default(),
            observed_store: ObservedStoreStats::default(),
            partial_bytes_pending: 0,
            shutdown_requested,
        };
        worker.publish_observations();
        Ok(worker)
    }

    fn run(
        &mut self,
        event_tx: &tokio_mpsc::Sender<WorkerEvent>,
        command_rx: &Receiver<WorkerCommand>,
    ) -> Result<(), WorkerError> {
        loop {
            if self.cancellation_requested() {
                return Ok(());
            }
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
                    self.next_maintenance_wait()
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
                    self.next_maintenance_wait()
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

            if self.process_discovery_message(event_tx, command_rx)? == LoopControl::Shutdown {
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

            let poll = self.readers_mut()?.poll(now);
            self.publish_reader_gauges();
            if self.cancellation_requested() {
                return Ok(());
            }
            let poll = poll?;
            let next_reader_probe = match poll {
                ReaderPoll::Cancelled => return Ok(()),
                ReaderPoll::Data(turn) => {
                    self.telemetry.add(
                        WorkerCounter::SourceBytesRead,
                        u64::try_from(turn.bytes().len()).unwrap_or(u64::MAX),
                    );
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
                    file_epoch,
                    committed_offset,
                    read_offset,
                    observed_size,
                    observed_fingerprint,
                } => {
                    if self.handle_truncation(
                        file_id,
                        file_epoch,
                        committed_offset,
                        read_offset,
                        observed_size,
                        observed_fingerprint,
                        event_tx,
                        command_rx,
                    )? == LoopControl::Shutdown
                    {
                        return Ok(());
                    }
                    continue;
                }
                ReaderPoll::EvictionRequired(request) => {
                    if self.open_batch_record_count()? != 0 {
                        self.readers_mut()?.defer_eviction(request)?;
                        if self.seal_open_batch(event_tx, command_rx)? == LoopControl::Shutdown {
                            return Ok(());
                        }
                    } else {
                        self.discard_framer(request.victim_file_id);
                        self.readers_mut()?.confirm_eviction(request)?;
                        self.publish_reader_gauges();
                    }
                    continue;
                }
                ReaderPoll::DescriptorCapacityBlocked { .. } => {
                    self.telemetry.add(WorkerCounter::DescriptorSaturation, 1);
                    if let Some(suppressed) = self.health_event(HealthEventCategory::Saturation) {
                        otel_warn!(
                            "filelog_receiver.descriptor_capacity_saturated",
                            suppressed_events = suppressed
                        );
                    }
                    None
                }
                ReaderPoll::RemovedWithoutDescriptor { file_id } => {
                    if self.cancellation_requested() {
                        return Ok(());
                    }
                    let Some(locator) = self.contain_removed_without_descriptor(file_id)? else {
                        return Ok(());
                    };
                    self.remember_inactive_locator(locator, file_id)?;
                    continue;
                }
                ReaderPoll::EvidenceUnstable { .. } => continue,
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
        if self.cancellation_requested() {
            return Ok(LoopControl::Shutdown);
        }
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
                let result = match self.commit_retained(batch_id, attempt) {
                    Ok(true) => Ok(()),
                    Ok(false) => return Ok(LoopControl::Shutdown),
                    Err(error) => Err(error),
                };
                let committed = result.is_ok();
                self.checkpoint_commit_failed = result.is_err();
                if result.is_ok() {
                    self.checkpoint_maintenance_failures = 0;
                    self.maintenance_retry_pending = false;
                }
                self.publish_observations();
                let event = WorkerEvent::CommitResult {
                    batch_id,
                    attempt,
                    explicit_loss,
                    result,
                };
                let handoff = send_event_interruptibly(event_tx, command_rx, event);
                let quarantine = if committed {
                    self.finish_pending_decode_quarantine()
                } else {
                    Ok(true)
                };
                let control = handoff?;
                if !quarantine? {
                    return Ok(LoopControl::Shutdown);
                }
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
        event_tx: &tokio_mpsc::Sender<WorkerEvent>,
        command_rx: &Receiver<WorkerCommand>,
    ) -> Result<LoopControl, WorkerError> {
        if self
            .pending_reconciliation_retry_at
            .is_some_and(|retry_at| Instant::now() < retry_at)
        {
            return Ok(LoopControl::Continue);
        }
        if let Some(batch) = self.pending_reconciliation.take() {
            self.pending_reconciliation_retry_at = None;
            return self.process_reconciliation(batch, event_tx, command_rx);
        }
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
            DiscoveryMessage::Batch(batch) => {
                self.observe_reconciliation(&batch);
                self.process_reconciliation(*batch, event_tx, command_rx)
            }
            DiscoveryMessage::Failed(error) => Err(WorkerError::Discovery(error)),
            DiscoveryMessage::Stopped => Err(WorkerError::DiscoveryStopped),
        }
    }

    fn observe_reconciliation(&mut self, batch: &ReconciliationBatch) {
        let stats = &batch.stats;
        self.telemetry
            .add(WorkerCounter::FilesDiscovered, stats.matched_paths);
        self.telemetry
            .add(WorkerCounter::FilesEligible, stats.eligible_candidates);
        self.telemetry.add(WorkerCounter::DiscoveryScans, 1);
        self.telemetry
            .add(WorkerCounter::DiscoveryScanErrors, stats.scan_errors);
        self.telemetry.add(
            WorkerCounter::DiscoveryScanDurationNs,
            duration_ns(stats.scan_duration),
        );
        self.telemetry.add(
            WorkerCounter::CandidateOverflow,
            stats.overflowed_candidates,
        );
        self.telemetry.add(
            WorkerCounter::CandidateOverflowScans,
            u64::from(stats.overflowed_candidates != 0),
        );
        self.telemetry.add(
            WorkerCounter::CandidateAdmissionDelayNs,
            duration_ns(stats.admission_delay),
        );
        self.telemetry
            .add(WorkerCounter::CandidateAdmissions, stats.admissions);
        self.telemetry.set(
            WorkerGauge::FilesPending,
            u64::try_from(stats.pending_candidates).unwrap_or(u64::MAX),
        );
        self.telemetry.set(
            WorkerGauge::CandidateOldestAgeNs,
            duration_ns(stats.oldest_pending_age),
        );
        self.telemetry.set(
            WorkerGauge::CandidateOverflowPersistenceNs,
            duration_ns(stats.overflow_persistence),
        );

        let mut observed = 0u64;
        let mut updated = 0u64;
        let mut removed = 0u64;
        for event in &batch.events {
            match event {
                CandidateEvent::Observed(_) => observed = observed.saturating_add(1),
                CandidateEvent::Updated(_) => updated = updated.saturating_add(1),
                CandidateEvent::Removed { .. } => removed = removed.saturating_add(1),
                CandidateEvent::Revoked { .. } => {}
            }
        }
        self.telemetry
            .add(WorkerCounter::DiscoveryObserved, observed);
        self.telemetry.add(WorkerCounter::DiscoveryUpdated, updated);
        self.telemetry.add(WorkerCounter::DiscoveryRemoved, removed);

        if stats.scan_errors != 0
            && let Some(suppressed) = self.health_event(HealthEventCategory::Scan)
        {
            let issue = stats
                .first_issue
                .as_ref()
                .map_or("unknown", |issue| match issue {
                    super::discovery::DiscoveryIssue::Io { .. } => "io",
                    super::discovery::DiscoveryIssue::Walk { .. } => "walk",
                    super::discovery::DiscoveryIssue::Identity(_) => "identity",
                    super::discovery::DiscoveryIssue::ConflictingPathRebind { .. } => {
                        "conflicting_path_rebind"
                    }
                });
            otel_warn!(
                "filelog_receiver.discovery_scan_failed",
                error_type = issue,
                error_count = stats.scan_errors,
                suppressed_events = suppressed
            );
        }
        if stats.overflowed_candidates != 0
            && let Some(suppressed) = self.health_event(HealthEventCategory::CandidateOverflow)
        {
            otel_warn!(
                "filelog_receiver.candidate_overflow",
                overflowed_candidates = stats.overflowed_candidates,
                pending_candidates = u64::try_from(stats.pending_candidates).unwrap_or(u64::MAX),
                suppressed_events = suppressed
            );
        }
    }

    fn process_reconciliation(
        &mut self,
        mut batch: ReconciliationBatch,
        event_tx: &tokio_mpsc::Sender<WorkerEvent>,
        command_rx: &Receiver<WorkerCommand>,
    ) -> Result<LoopControl, WorkerError> {
        if self.cancellation_requested() {
            return Ok(LoopControl::Shutdown);
        }
        if self.freeze_policy_revocations(&batch.events)? {
            self.defer_reconciliation(batch, None)?;
            return self.seal_open_batch(event_tx, command_rx);
        }
        let refreshed = self.refresh_updated_candidates(&mut batch);
        if self.cancellation_requested() {
            return Ok(LoopControl::Shutdown);
        }
        if !refreshed? {
            let retry_at = Instant::now()
                .checked_add(COMMAND_POLL_INTERVAL)
                .ok_or(WorkerError::ReconciliationRetryDeadlineOverflow)?;
            self.defer_reconciliation(batch, Some(retry_at))?;
            return Ok(LoopControl::Continue);
        }
        if self.cancellation_requested() {
            return Ok(LoopControl::Shutdown);
        }
        self.detect_updated_truncations(&batch.events)?;
        if self.cancellation_requested() {
            return Ok(LoopControl::Shutdown);
        }
        for truncation in &self.detected_truncations {
            if self
                .open_batch
                .as_ref()
                .ok_or(WorkerError::MissingOpenBatch {
                    operation: "checking truncation overlap",
                })?
                .progress_frontier(truncation.file_id)
                .is_some()
            {
                self.defer_reconciliation(batch, None)?;
                self.detected_truncations.clear();
                return self.seal_open_batch(event_tx, command_rx);
            }
        }
        if !self.apply_detected_truncations()? {
            return Ok(LoopControl::Shutdown);
        }
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
        // A locator's candidate path is a confirmed distinguished-binding
        // decision only when discovery already had continuity memory for
        // it (an `Updated` event); a locator discovery is observing for the
        // first time since process start (an `Observed` event) must not
        // silently overwrite an already-durable `advisory_path` (see
        // `docs/filelog-receiver-phase1-spec.md`, "Discovery and
        // matching").
        let mut confirmed_path_bindings = HashSet::with_capacity(candidate_count);
        for event in &batch.events {
            match event {
                CandidateEvent::Observed(candidate) => {
                    self.candidate_evidence.push(candidate.evidence.clone());
                }
                CandidateEvent::Updated(candidate) => {
                    self.candidate_evidence.push(candidate.evidence.clone());
                    let _ = confirmed_path_bindings.insert(candidate.evidence.locator);
                }
                CandidateEvent::Removed { .. } | CandidateEvent::Revoked { .. } => {}
            }
        }
        let now_unix_nano = unix_nanos()?.1;
        if self.cancellation_requested() {
            return Ok(LoopControl::Shutdown);
        }
        let shutdown_requested = Arc::clone(&self.shutdown_requested);
        let mut plan = plan_with_admission(
            &self.store,
            &self.candidate_evidence,
            &batch.inventory,
            &self.identity_settings,
            now_unix_nano,
            &batch.recognized_replacements,
            &confirmed_path_bindings,
        )
        .map_err(WorkerError::Identity)?;
        if self.cancellation_requested() {
            return Ok(LoopControl::Shutdown);
        }
        let Some(resolved) =
            self.retry_direct_checkpoint_operation("persist identity reconciliation", |runtime| {
                plan.persist_cancellable(&mut runtime.store, || {
                    shutdown_requested.load(Ordering::Acquire)
                })
                .map_err(WorkerError::Identity)
            })?
        else {
            return Ok(LoopControl::Shutdown);
        };
        let Some(resolved) = resolved else {
            return Ok(LoopControl::Shutdown);
        };
        if self.cancellation_requested() {
            return Ok(LoopControl::Shutdown);
        }
        if resolved.len() != self.candidate_evidence.len() {
            return Err(WorkerError::Inconsistent {
                reason: "identity resolution count differs from candidate event count",
            });
        }
        for resolution in &resolved {
            match resolution {
                IdentityResolution::Deferred => {
                    self.telemetry.add(WorkerCounter::TrackedSaturation, 1);
                    self.telemetry.set(WorkerGauge::TrackedSaturated, 1);
                    if let Some(suppressed) = self.health_event(HealthEventCategory::Saturation) {
                        otel_warn!(
                            "filelog_receiver.tracked_table_saturated",
                            suppressed_events = suppressed
                        );
                    }
                }
                IdentityResolution::Resolved(identity) => match identity.matched_by {
                    IdentityMatch::ExactLocator => {
                        self.telemetry.add(WorkerCounter::IdentityExactMatches, 1);
                    }
                    IdentityMatch::NewDiscovery => {
                        self.telemetry.add(WorkerCounter::IdentityRegistrations, 1);
                    }
                    IdentityMatch::RecoveryMismatch => {
                        self.telemetry.add(WorkerCounter::IdentityRegistrations, 1);
                        self.telemetry.add(WorkerCounter::IdentityResets, 1);
                        self.telemetry
                            .add(WorkerCounter::IdentityRecoveryMismatches, 1);
                        if identity.lifecycle_state == LifecycleState::Quarantined {
                            self.telemetry
                                .add(WorkerCounter::QuarantineRecoveryMismatch, 1);
                        }
                    }
                    IdentityMatch::RecognizedReplacement => {
                        self.telemetry.add(WorkerCounter::IdentityRegistrations, 1);
                        self.telemetry
                            .add(WorkerCounter::RotationRecognizedReplacement, 1);
                    }
                },
            }
        }

        let feedback_capacity = batch
            .events
            .len()
            .checked_add(self.pending_finalizations.len())
            .ok_or(WorkerError::Inconsistent {
                reason: "discovery feedback capacity overflowed",
            })?;
        let mut feedback = feedback_with_capacity(feedback_capacity)?;
        feedback.finalized.append(&mut self.pending_finalizations);
        let mut resolved = resolved.into_iter();
        for event in batch.events {
            if self.cancellation_requested() {
                return Ok(LoopControl::Shutdown);
            }
            match event {
                CandidateEvent::Observed(candidate) => {
                    let resolution = next_resolution(&mut resolved)?;
                    let IdentityResolution::Resolved(identity) = resolution else {
                        feedback.deferred.push(candidate.evidence.locator);
                        continue;
                    };
                    let locator = candidate.evidence.locator;
                    let advisory_path = identity.advisory_path.clone();
                    if identity.lifecycle_state != LifecycleState::Active {
                        if let Some(existing) =
                            self.inactive_locators.insert(locator, identity.file_id)
                            && existing != identity.file_id
                        {
                            return Err(WorkerError::Inconsistent {
                                reason: "inactive locator changed durable identity",
                            });
                        }
                        feedback.durable.push(DurableAck {
                            locator,
                            advisory_path,
                        });
                        continue;
                    }
                    if let Some(detached_file_id) = self.inactive_locators.get(&locator)
                        && *detached_file_id != identity.file_id
                    {
                        return Err(WorkerError::Inconsistent {
                            reason: "observed locator changed detached durable identity",
                        });
                    }
                    let file_id = identity.file_id;
                    let insert_result = self.readers_mut()?.insert(candidate, identity);
                    self.record_runtime_lease_observations()?;
                    match insert_result {
                        Ok(()) => {
                            let _detached = self.inactive_locators.remove(&locator);
                            feedback.durable.push(DurableAck {
                                locator,
                                advisory_path,
                            });
                        }
                        Err(ReaderError::ReaderCapacityExhausted { .. }) => {
                            self.remember_inactive_locator(locator, file_id)?;
                            feedback.deferred.push(locator);
                        }
                        Err(error) => {
                            self.observe_reader_error(&error);
                            return Err(WorkerError::Reader(error));
                        }
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
                    let advisory_path = identity.advisory_path.clone();
                    if identity.lifecycle_state == LifecycleState::Active {
                        match self.readers_ref()?.file_id_for_locator(locator) {
                            Ok(file_id) => {
                                if file_id != identity.file_id {
                                    return Err(WorkerError::Inconsistent {
                                        reason: "updated locator resolved to a different reader",
                                    });
                                }
                                let path_changed = self
                                    .readers_ref()?
                                    .record_context(identity.file_id)?
                                    .matched_path
                                    != candidate.matched_path;
                                self.readers_mut()?.update(candidate, &identity)?;
                                if path_changed {
                                    self.telemetry.add(WorkerCounter::RotationMoveCreate, 1);
                                }
                            }
                            Err(ReaderError::UnknownLocator { .. }) => {
                                let detached_file_id = *self
                                    .inactive_locators
                                    .get(&locator)
                                    .ok_or(WorkerError::Inconsistent {
                                        reason: "re-eligible locator has no detached durable identity",
                                    })?;
                                if detached_file_id != identity.file_id {
                                    return Err(WorkerError::Inconsistent {
                                        reason: "re-eligible locator changed detached durable identity",
                                    });
                                }
                                let insert_result = self.readers_mut()?.insert(candidate, identity);
                                self.record_runtime_lease_observations()?;
                                if let Err(error) = insert_result {
                                    self.observe_reader_error(&error);
                                    return Err(WorkerError::Reader(error));
                                }
                                let removed = self.inactive_locators.remove(&locator);
                                debug_assert_eq!(removed, Some(detached_file_id));
                            }
                            Err(error) => return Err(WorkerError::Reader(error)),
                        }
                    } else if let Some(existing) =
                        self.inactive_locators.insert(locator, identity.file_id)
                        && existing != identity.file_id
                    {
                        return Err(WorkerError::Inconsistent {
                            reason: "inactive locator update changed durable identity",
                        });
                    }
                    feedback.durable.push(DurableAck {
                        locator,
                        advisory_path,
                    });
                }
                CandidateEvent::Removed { locator } => {
                    match self.readers_ref()?.file_id_for_locator(locator) {
                        Ok(file_id) => match self.readers_mut()?.mark_removed(locator)? {
                            RemovalDisposition::HandleRetained => {}
                            RemovalDisposition::DescriptorAbsent => {
                                let Some(released) =
                                    self.contain_removed_without_descriptor(file_id)?
                                else {
                                    return Ok(LoopControl::Shutdown);
                                };
                                if released != locator {
                                    return Err(WorkerError::Inconsistent {
                                        reason: "removed reader containment released a different locator",
                                    });
                                }
                                feedback.finalized.push(locator);
                            }
                        },
                        Err(ReaderError::UnknownLocator { .. }) => {
                            if self.inactive_locators.remove(&locator).is_none() {
                                return Err(WorkerError::Inconsistent {
                                    reason: "removed locator belongs to neither a reader nor detached state",
                                });
                            }
                            feedback.finalized.push(locator);
                        }
                        Err(error) => return Err(WorkerError::Reader(error)),
                    }
                }
                CandidateEvent::Revoked { locator, reason: _ } => {
                    match self.readers_ref()?.file_id_for_locator(locator) {
                        Ok(file_id) => {
                            self.readers_ref()?.preflight_release_revoked(file_id)?;
                            self.readers_mut()?.pause(file_id)?;
                            let contributed = self
                                .open_batch
                                .as_ref()
                                .ok_or(WorkerError::MissingOpenBatch {
                                    operation: "checking policy revocation overlap",
                                })?
                                .progress_frontier(file_id)
                                .is_some();
                            if contributed {
                                return Err(WorkerError::Inconsistent {
                                    reason: "policy revocation still overlaps an open batch after preemption",
                                });
                            } else {
                                self.discard_framer(file_id);
                                let released = self.readers_mut()?.release_revoked(file_id)?;
                                if released != locator {
                                    return Err(WorkerError::Inconsistent {
                                        reason: "policy revocation released a different locator",
                                    });
                                }
                                let _ = self.rotation_waits.remove(&file_id);
                                let _ = self.record_numbers.remove(file_id);
                                self.remove_drain_file(file_id);
                                self.remember_inactive_locator(locator, file_id)?;
                                feedback.revoked.push(locator);
                            }
                        }
                        Err(ReaderError::UnknownLocator { .. }) => {
                            feedback.revoked.push(locator);
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
        self.publish_observations();
        self.send_feedback_interruptibly(feedback, command_rx)
    }

    fn observe_reader_error(&mut self, error: &ReaderError) {
        let ReaderError::Lease(lease_error) = error else {
            return;
        };
        if let Some(suppressed) = self.health_event(HealthEventCategory::Lease) {
            let error_type = match lease_error {
                LeaseError::Contended { .. } => "contended",
                LeaseError::UnspecifiedLocator => "unspecified_locator",
                LeaseError::ScopeCapacityExhausted { .. }
                | LeaseError::ScopeCapacityTooLarge { .. }
                | LeaseError::ReceiverScopeCapacityExhausted { .. }
                | LeaseError::LocatorBucketCapacityExhausted { .. }
                | LeaseError::AggregateCapacityOverflow => "capacity",
                LeaseError::AllocationFailed { .. } => "allocation",
                LeaseError::RegistryPoisoned | LeaseError::RegistryInconsistent => "integrity",
                LeaseError::OutstandingLeases { .. } | LeaseError::ScopeClosed => "lifecycle",
            };
            otel_warn!(
                "filelog_receiver.runtime_lease_failed",
                error_type = error_type,
                suppressed_events = suppressed
            );
        }
    }

    fn freeze_policy_revocations(
        &mut self,
        events: &[CandidateEvent],
    ) -> Result<bool, WorkerError> {
        let mut overlaps_open_batch = false;
        for event in events {
            let CandidateEvent::Revoked { locator, .. } = event else {
                continue;
            };
            match self.readers_ref()?.file_id_for_locator(*locator) {
                Ok(file_id) => {
                    self.readers_ref()?.preflight_release_revoked(file_id)?;
                    self.readers_mut()?.pause(file_id)?;
                    overlaps_open_batch |= self
                        .open_batch
                        .as_ref()
                        .ok_or(WorkerError::MissingOpenBatch {
                            operation: "preempting policy revocation",
                        })?
                        .progress_frontier(file_id)
                        .is_some();
                }
                Err(ReaderError::UnknownLocator { .. }) => {
                    // The matching Observed transition may still be awaiting
                    // durable/deferred feedback, in which case no reader
                    // exists yet and there is nothing runtime-owned to pause.
                }
                Err(error) => return Err(WorkerError::Reader(error)),
            }
        }
        Ok(overlaps_open_batch)
    }

    fn record_runtime_lease_observations(&mut self) -> Result<(), WorkerError> {
        let lease = self.readers_mut()?.take_lease_observations();
        self.telemetry
            .add(WorkerCounter::RuntimeLeaseWaits, lease.attempts);
        self.telemetry
            .add(WorkerCounter::RuntimeLeaseWaitNs, lease.wait_ns);
        self.telemetry
            .add(WorkerCounter::RuntimeLeaseFailures, lease.failures);
        self.telemetry
            .add(WorkerCounter::RuntimeLeaseContentions, lease.contentions);
        Ok(())
    }

    fn refresh_updated_candidates(
        &mut self,
        batch: &mut ReconciliationBatch,
    ) -> Result<bool, WorkerError> {
        for event in &mut batch.events {
            if self.cancellation_requested() {
                return Ok(false);
            }
            let CandidateEvent::Updated(candidate) = event else {
                continue;
            };
            match self
                .readers_ref()?
                .file_id_for_locator(candidate.evidence.locator)
            {
                Ok(_) => {}
                Err(ReaderError::UnknownLocator { .. }) => continue,
                Err(error) => return Err(WorkerError::Reader(error)),
            }
            let previous_fingerprint = candidate.evidence.fingerprint.clone();
            let refreshed = self.readers_ref()?.refresh_candidate_evidence(candidate);
            if self.cancellation_requested() {
                return Ok(false);
            }
            match refreshed? {
                CandidateEvidenceRefresh::Cancelled => return Ok(false),
                CandidateEvidenceRefresh::Refreshed => {
                    batch.inventory.replace_fingerprint_observation(
                        candidate.evidence.locator,
                        &previous_fingerprint,
                        &candidate.evidence.fingerprint,
                        self.identity_settings.fingerprint_bytes,
                    )?;
                }
                CandidateEvidenceRefresh::DescriptorAbsent => {}
                CandidateEvidenceRefresh::Retry => return Ok(false),
            }
        }
        Ok(true)
    }

    fn defer_reconciliation(
        &mut self,
        batch: ReconciliationBatch,
        retry_at: Option<Instant>,
    ) -> Result<(), WorkerError> {
        if self.pending_reconciliation.is_some() || self.pending_reconciliation_retry_at.is_some() {
            return Err(WorkerError::Inconsistent {
                reason: "more than one reconciliation batch was deferred",
            });
        }
        self.pending_reconciliation = Some(batch);
        self.pending_reconciliation_retry_at = retry_at;
        Ok(())
    }

    fn detect_updated_truncations(&mut self, events: &[CandidateEvent]) -> Result<(), WorkerError> {
        self.detected_truncations.clear();
        for event in events {
            if self.cancellation_requested() {
                self.detected_truncations.clear();
                return Ok(());
            }
            let CandidateEvent::Updated(candidate) = event else {
                continue;
            };
            let locator = candidate.evidence.locator;
            let (
                file_id,
                file_epoch,
                committed_offset,
                read_offset,
                durable_fingerprint,
                quarantined,
                present,
            ) = match self.readers_ref()?.identity_context(locator) {
                Ok(context) => {
                    let frontier = self.readers_ref()?.frontier(context.file_id)?;
                    (
                        context.file_id,
                        context.file_epoch,
                        context.committed_offset,
                        context.read_offset,
                        context.durable_fingerprint.to_vec(),
                        false,
                        frontier.present,
                    )
                }
                Err(ReaderError::UnknownLocator { .. }) => {
                    let file_id = *self.inactive_locators.get(&locator).ok_or(
                            WorkerError::Inconsistent {
                                reason: "Updated locator belongs to neither a reader nor detached state",
                            },
                        )?;
                    let record = self
                        .store
                        .table()
                        .get(&file_id)
                        .ok_or(WorkerError::MissingCheckpointRecord { file_id })?;
                    (
                        file_id,
                        record.file_epoch,
                        record.committed_offset,
                        record.committed_offset,
                        record.fingerprint.clone(),
                        record.lifecycle_state == LifecycleState::Quarantined,
                        true,
                    )
                }
                Err(error) => return Err(WorkerError::Reader(error)),
            };

            if self.store.table().live_file_id_for_locator(locator) != Some(file_id) {
                return Err(WorkerError::UnsupportedUpdatedIdentity {
                    locator,
                    file_id,
                    reason: "reader association does not match the sole live locator record",
                });
            }
            if quarantined {
                continue;
            }
            if candidate.evidence.size < read_offset
                || !candidate
                    .evidence
                    .fingerprint
                    .starts_with(&durable_fingerprint)
            {
                if self.detected_truncations.len() == self.detected_truncations.capacity() {
                    return Err(WorkerError::Inconsistent {
                        reason: "detected truncations exceed their configured bound",
                    });
                }
                self.detected_truncations.push(DetectedTruncation {
                    file_id,
                    expected_file_epoch: file_epoch,
                    expected_committed_offset: committed_offset,
                    observed_size: candidate.evidence.size,
                    observed_fingerprint: candidate.evidence.fingerprint.clone(),
                    locator,
                    present,
                });
            }
        }
        Ok(())
    }

    fn apply_detected_truncations(&mut self) -> Result<bool, WorkerError> {
        for index in 0..self.detected_truncations.len() {
            if self.cancellation_requested() {
                self.detected_truncations.clear();
                return Ok(false);
            }
            let truncation = self.detected_truncations[index].clone();
            self.preflight_truncation(&truncation)?;
        }
        for index in 0..self.detected_truncations.len() {
            if self.cancellation_requested() {
                self.detected_truncations.clear();
                return Ok(false);
            }
            let truncation = self.detected_truncations[index].clone();
            if !self.apply_truncation(truncation)? {
                self.detected_truncations.clear();
                return Ok(false);
            }
        }
        self.detected_truncations.clear();
        Ok(true)
    }

    fn preflight_truncation(&self, truncation: &DetectedTruncation) -> Result<(), WorkerError> {
        if self.retained.is_some() {
            return Err(WorkerError::Inconsistent {
                reason: "truncate transition cannot run under a retained batch",
            });
        }
        if self
            .open_batch
            .as_ref()
            .ok_or(WorkerError::MissingOpenBatch {
                operation: "preflighting truncation",
            })?
            .progress_frontier(truncation.file_id)
            .is_some()
        {
            return Err(WorkerError::Inconsistent {
                reason: "truncate transition overlaps an unacknowledged file delta",
            });
        }
        let record = self.store.table().get(&truncation.file_id).ok_or(
            WorkerError::MissingCheckpointRecord {
                file_id: truncation.file_id,
            },
        )?;
        if record.lifecycle_state != LifecycleState::Active
            || record.file_epoch != truncation.expected_file_epoch
            || record.committed_offset != truncation.expected_committed_offset
            || record.locator != truncation.locator
        {
            return Err(WorkerError::Inconsistent {
                reason: "truncate evidence no longer matches durable active state",
            });
        }
        match self.config.rotation.on_truncate {
            OnTruncate::Fail => self
                .readers_ref()?
                .preflight_release_revoked(truncation.file_id)?,
            OnTruncate::ReadNew => {
                let _resulting_epoch = truncation.expected_file_epoch.checked_add(1).ok_or(
                    WorkerError::Inconsistent {
                        reason: "file epoch overflowed during truncate reset",
                    },
                )?;
                self.readers_ref()?.preflight_truncate_reset(
                    truncation.file_id,
                    truncation.expected_file_epoch,
                    truncation.expected_committed_offset,
                )?;
            }
        }
        Ok(())
    }

    fn apply_truncation(&mut self, truncation: DetectedTruncation) -> Result<bool, WorkerError> {
        record_truncation_detection(&self.telemetry);
        let reset_time_unix_nano = unix_nanos()?.1;
        match self.config.rotation.on_truncate {
            OnTruncate::Fail => {
                let mut quarantines = reserved_vec(1, "truncate quarantine operation")?;
                quarantines.push(QuarantineFile {
                    file_id: truncation.file_id,
                    expected_file_epoch: truncation.expected_file_epoch,
                    reason_code: QUARANTINE_REASON_TRUNCATE,
                    locator: truncation.locator,
                    observed_size: truncation.observed_size,
                    quarantine_epoch: truncation.expected_file_epoch,
                    quarantine_time_unix_nano: reset_time_unix_nano,
                });
                if self.cancellation_requested() {
                    return Ok(false);
                }
                let Some(()) = self.retry_direct_checkpoint_operation(
                    "persist a truncate quarantine",
                    |runtime| {
                        let _outcomes = runtime
                            .store
                            .quarantine_files(quarantines.clone())
                            .map_err(WorkerError::Store)?;
                        runtime.store.sync().map_err(WorkerError::Store)
                    },
                )?
                else {
                    return Ok(false);
                };
                record_truncation_outcome(&self.telemetry, OnTruncate::Fail);
                if let Some(suppressed) = self.health_event(HealthEventCategory::Truncation) {
                    otel_warn!(
                        "filelog_receiver.copytruncate_quarantined",
                        policy = "fail",
                        observed_size = truncation.observed_size,
                        suppressed_events = suppressed
                    );
                }
                self.publish_observations();

                let released = self.readers_mut()?.release_revoked(truncation.file_id)?;
                if released != truncation.locator {
                    return Err(WorkerError::Inconsistent {
                        reason: "quarantined reader released a different locator",
                    });
                }
                self.discard_framer(truncation.file_id);
                let _ = self.rotation_waits.remove(&truncation.file_id);
                let _ = self.record_numbers.remove(truncation.file_id);
                self.remove_drain_file(truncation.file_id);
                if truncation.present {
                    self.remember_inactive_locator(truncation.locator, truncation.file_id)?;
                } else {
                    self.queue_finalization_feedback(truncation.locator)?;
                }
            }
            OnTruncate::ReadNew => {
                let resulting_epoch = truncation.expected_file_epoch.checked_add(1).ok_or(
                    WorkerError::Inconsistent {
                        reason: "file epoch overflowed during truncate reset",
                    },
                )?;
                let mut operations = reserved_vec(1, "truncate reset transaction")?;
                operations.push(Operation::ResetAfterTruncate(ResetAfterTruncate {
                    file_id: truncation.file_id,
                    expected_active_epoch: truncation.expected_file_epoch,
                    observed_truncated_size: truncation.observed_size,
                    resulting_epoch,
                    new_committed_offset: 0,
                    new_framing_resume: FramingResume::Clean,
                    new_fingerprint: truncation.observed_fingerprint.clone(),
                    reset_time_unix_nano,
                    reason_code: TRUNCATE_RESET_REASON_READ_NEW,
                }));
                if self.cancellation_requested() {
                    return Ok(false);
                }
                let Some(()) = self.retry_direct_checkpoint_operation(
                    "persist a truncate reset",
                    |runtime| {
                        let _outcome = runtime
                            .store
                            .append(operations.clone())
                            .map_err(WorkerError::Store)?;
                        runtime.store.sync().map_err(WorkerError::Store)
                    },
                )?
                else {
                    return Ok(false);
                };
                record_truncation_outcome(&self.telemetry, OnTruncate::ReadNew);
                if let Some(suppressed) = self.health_event(HealthEventCategory::Truncation) {
                    otel_warn!(
                        "filelog_receiver.copytruncate_reset",
                        policy = "read_new",
                        observed_size = truncation.observed_size,
                        suppressed_events = suppressed
                    );
                }
                self.publish_observations();

                let resume = !self.drain_requested;
                self.readers_mut()?.apply_preflighted_truncate_reset(
                    truncation.file_id,
                    truncation.expected_file_epoch,
                    truncation.expected_committed_offset,
                    resulting_epoch,
                    truncation.observed_fingerprint,
                    resume,
                )?;
                self.discard_framer(truncation.file_id);
                let _ = self.rotation_waits.remove(&truncation.file_id);
                let _ = self.record_numbers.remove(truncation.file_id);
                self.remove_drain_file(truncation.file_id);
            }
        }
        self.publish_observations();
        Ok(true)
    }

    fn contain_removed_without_descriptor(
        &mut self,
        file_id: FileId,
    ) -> Result<Option<Locator>, WorkerError> {
        if self.retained.is_some()
            || self
                .open_batch
                .as_ref()
                .ok_or(WorkerError::MissingOpenBatch {
                    operation: "containing descriptor-free rotation",
                })?
                .progress_frontier(file_id)
                .is_some()
        {
            return Err(WorkerError::Inconsistent {
                reason: "descriptor-free rotation overlaps unacknowledged progress",
            });
        }
        let frontier = self.readers_ref()?.frontier(file_id)?;
        if frontier.present || frontier.descriptor_resident {
            return Err(WorkerError::Inconsistent {
                reason: "descriptor-free rotation containment received a present or resident reader",
            });
        }
        self.readers_ref()?.preflight_release_revoked(file_id)?;
        let (committed_offset, locator) = {
            let record = self
                .store
                .table()
                .get(&file_id)
                .ok_or(WorkerError::MissingCheckpointRecord { file_id })?;
            if record.lifecycle_state != LifecycleState::Active {
                return Err(WorkerError::InactiveCheckpointRecord {
                    file_id,
                    state: record.lifecycle_state,
                });
            }
            (record.committed_offset, record.locator)
        };
        if self.cancellation_requested() {
            return Ok(None);
        }
        record_descriptor_unavailable_telemetry(&self.telemetry);
        if let Some(suppressed) = self.health_event(HealthEventCategory::Rotation) {
            otel_warn!(
                "filelog_receiver.rotation_descriptor_unavailable",
                committed_offset,
                durable_state = "active_unfinalized",
                suppressed_events = suppressed
            );
        }
        self.publish_observations();

        let released = self.readers_mut()?.release_revoked(file_id)?;
        if released != locator {
            return Err(WorkerError::Inconsistent {
                reason: "descriptor-free rotation released a different locator",
            });
        }
        self.discard_framer(file_id);
        let _ = self.rotation_waits.remove(&file_id);
        self.remove_drain_file(file_id);
        Ok(Some(locator))
    }

    fn handle_truncation(
        &mut self,
        file_id: FileId,
        file_epoch: u32,
        committed_offset: u64,
        read_offset: u64,
        observed_size: u64,
        observed_fingerprint: Vec<u8>,
        event_tx: &tokio_mpsc::Sender<WorkerEvent>,
        command_rx: &Receiver<WorkerCommand>,
    ) -> Result<LoopControl, WorkerError> {
        if self
            .open_batch
            .as_ref()
            .ok_or(WorkerError::MissingOpenBatch {
                operation: "checking a polled truncation",
            })?
            .progress_frontier(file_id)
            .is_some()
        {
            return self.seal_open_batch(event_tx, command_rx);
        }
        let frontier = self.readers_ref()?.frontier(file_id)?;
        if frontier.file_epoch != file_epoch
            || frontier.committed_offset != committed_offset
            || frontier.read_offset != read_offset
        {
            return Err(WorkerError::Truncation {
                file_id,
                committed_offset,
                read_offset,
                observed_size,
            });
        }
        let truncation = DetectedTruncation {
            file_id,
            expected_file_epoch: file_epoch,
            expected_committed_offset: committed_offset,
            observed_size,
            observed_fingerprint,
            locator: self.readers_ref()?.locator(file_id)?,
            present: frontier.present,
        };
        self.preflight_truncation(&truncation)?;
        if self.cancellation_requested() {
            return Ok(LoopControl::Shutdown);
        }
        if !self.apply_truncation(truncation)? {
            return Ok(LoopControl::Shutdown);
        }
        Ok(LoopControl::Continue)
    }

    fn remove_drain_file(&mut self, file_id: FileId) {
        let _ = self.drain_limits.remove(&file_id);
        if let Some(index) = self
            .drain_order
            .iter()
            .position(|candidate| *candidate == file_id)
        {
            let _ = self.drain_order.remove(index);
        }
    }

    fn queue_finalization_feedback(&mut self, locator: Locator) -> Result<(), WorkerError> {
        if self.pending_finalizations.contains(&locator) {
            return Err(WorkerError::Inconsistent {
                reason: "locator was queued twice for discovery finalization",
            });
        }
        if self.pending_finalizations.len() == self.pending_finalizations.capacity() {
            return Err(WorkerError::Inconsistent {
                reason: "pending discovery finalizations exceed their configured bound",
            });
        }
        self.pending_finalizations.push(locator);
        Ok(())
    }

    fn remember_inactive_locator(
        &mut self,
        locator: Locator,
        file_id: FileId,
    ) -> Result<(), WorkerError> {
        if let Some(existing) = self.inactive_locators.insert(locator, file_id)
            && existing != file_id
        {
            return Err(WorkerError::Inconsistent {
                reason: "detached locator changed durable identity",
            });
        }
        Ok(())
    }

    fn send_feedback_interruptibly(
        &mut self,
        mut feedback: DiscoveryFeedback,
        command_rx: &Receiver<WorkerCommand>,
    ) -> Result<LoopControl, WorkerError> {
        loop {
            if self.cancellation_requested() {
                return Ok(LoopControl::Shutdown);
            }
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
        if self.rotation_waits.remove(&file_id).is_some() {
            self.telemetry.add(WorkerCounter::RotationLateWrites, 1);
            if let Some(suppressed) = self.health_event(HealthEventCategory::Rotation) {
                otel_info!(
                    "filelog_receiver.rotation_late_write",
                    suppressed_events = suppressed
                );
            }
        }
        let base = FramerBase {
            file_epoch: turn.file_epoch(),
            committed_offset: turn.committed_offset(),
            framing_resume: turn.framing_resume(),
        };
        let mut active = match self.take_framer(file_id) {
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
                let seed_window = match self
                    .readers_mut()?
                    .committed_frontier_window(file_id, base.committed_offset)
                {
                    Ok(window) => window,
                    Err(error) => {
                        self.readers_mut()?
                            .complete_turn(turn, 0, TurnDisposition::Paused)?;
                        return Err(WorkerError::Reader(error));
                    }
                };
                let framer = match Framer::from_runtime(
                    file_id,
                    base.file_epoch,
                    &self.config,
                    base.committed_offset,
                    base.framing_resume,
                    base.committed_offset == 0,
                    seed_window,
                    now,
                ) {
                    Ok(framer) => framer,
                    Err(error) => {
                        self.readers_mut()?
                            .complete_turn(turn, 0, TurnDisposition::Paused)?;
                        return Err(WorkerError::Framer(error));
                    }
                };
                ActiveFramer {
                    framer,
                    base,
                    pending_bytes: 0,
                }
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
                self.put_framer(file_id, active)?;
                Ok(LoopControl::Continue)
            }
            Ok((consumed, control @ (AppendControl::SealBefore | AppendControl::SealAfter))) => {
                debug_assert!(control.requires_seal());
                self.readers_mut()?
                    .complete_turn(turn, consumed, TurnDisposition::Paused)?;
                self.seal_open_batch(event_tx, command_rx)
            }
            Err(failure) => {
                let fatal_decode_size = match failure.error.as_ref() {
                    WorkerError::Framer(error) => {
                        self.observe_framer_error(error);
                        match error {
                            FramerError::Decode(DecodeError::FatalMalformed { range, .. }) => {
                                Some(range.end)
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };
                self.readers_mut()?.complete_turn(
                    turn,
                    failure.consumed,
                    TurnDisposition::Paused,
                )?;
                if let Some(observed_size) = fatal_decode_size {
                    self.discard_framer(file_id);
                    if self.cancellation_requested() {
                        return Ok(LoopControl::Shutdown);
                    }
                    if self.open_batch_record_count()? != 0 {
                        if self
                            .pending_decode_quarantine
                            .replace(PendingDecodeQuarantine {
                                file_id,
                                observed_size,
                            })
                            .is_some()
                        {
                            return Err(WorkerError::Inconsistent {
                                reason: "multiple decode quarantines overlap one retained batch",
                            });
                        }
                        return self.seal_open_batch(event_tx, command_rx);
                    }
                    if !self.quarantine_decode_failure(file_id, observed_size)? {
                        return Ok(LoopControl::Shutdown);
                    }
                    return Ok(LoopControl::Continue);
                }
                Err(*failure.error)
            }
        }
    }

    fn quarantine_decode_failure(
        &mut self,
        file_id: FileId,
        observed_size: u64,
    ) -> Result<bool, WorkerError> {
        if self.retained.is_some() || self.open_batch_record_count()? != 0 {
            return Err(WorkerError::Inconsistent {
                reason: "decode quarantine overlaps provisional batch progress",
            });
        }
        self.readers_ref()?.preflight_release_revoked(file_id)?;
        let frontier = self.readers_ref()?.frontier(file_id)?;
        let (file_epoch, locator) = {
            let record = self
                .store
                .table()
                .get(&file_id)
                .ok_or(WorkerError::MissingCheckpointRecord { file_id })?;
            if record.lifecycle_state != LifecycleState::Active {
                return Err(WorkerError::InactiveCheckpointRecord {
                    file_id,
                    state: record.lifecycle_state,
                });
            }
            (record.file_epoch, record.locator)
        };
        let mut quarantines = reserved_vec(1, "decode quarantine operation")?;
        quarantines.push(QuarantineFile {
            file_id,
            expected_file_epoch: file_epoch,
            reason_code: QUARANTINE_REASON_DECODE,
            locator,
            observed_size,
            quarantine_epoch: file_epoch,
            quarantine_time_unix_nano: unix_nanos()?.1,
        });
        if self.cancellation_requested() {
            return Ok(false);
        }
        let Some(()) =
            self.retry_direct_checkpoint_operation("persist a decode quarantine", |runtime| {
                let _outcomes = runtime
                    .store
                    .quarantine_files(quarantines.clone())
                    .map_err(WorkerError::Store)?;
                runtime.store.sync().map_err(WorkerError::Store)
            })?
        else {
            return Ok(false);
        };
        self.telemetry.add(WorkerCounter::QuarantineDecode, 1);
        if let Some(suppressed) = self.health_event(HealthEventCategory::Quarantine) {
            otel_warn!(
                "filelog_receiver.decode_quarantined",
                policy = "fail",
                suppressed_events = suppressed
            );
        }
        self.publish_observations();

        let released = self.readers_mut()?.release_revoked(file_id)?;
        if released != locator {
            return Err(WorkerError::Inconsistent {
                reason: "decode quarantine released a different locator",
            });
        }
        self.discard_framer(file_id);
        let _ = self.rotation_waits.remove(&file_id);
        let _ = self.record_numbers.remove(file_id);
        self.remove_drain_file(file_id);
        if frontier.present {
            self.remember_inactive_locator(locator, file_id)?;
        } else {
            self.queue_finalization_feedback(locator)?;
        }
        Ok(true)
    }

    fn finish_pending_decode_quarantine(&mut self) -> Result<bool, WorkerError> {
        let Some(pending) = self.pending_decode_quarantine.take() else {
            return Ok(true);
        };
        self.quarantine_decode_failure(pending.file_id, pending.observed_size)
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
            let step = match active.framer.step(input, ready_at) {
                Ok(step) => step,
                Err(error) => {
                    return Err(TurnFailure {
                        consumed,
                        error: Box::new(WorkerError::Framer(error)),
                    });
                }
            };
            self.observe_framer_counters(&mut active.framer);
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
                    control @ (AppendControl::SealBefore | AppendControl::SealAfter) => {
                        return Ok((consumed, control));
                    }
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
        let mut active = self.take_framer(file_id);
        if let Some(active) = active.as_mut() {
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
                self.observe_framer_counters(&mut active.framer);
                let produced = step.output.is_some();
                if let Some(record) = step.output {
                    let control = self.append_record(file_id, active.base, record, ready_at)?;
                    if control.requires_seal() {
                        return self.seal_open_batch(event_tx, command_rx);
                    }
                }
                if !produced {
                    break;
                }
            }
        }

        if !self.rotation_finalization_due(file_id, now)? {
            if let Some(active) = active {
                self.put_framer(file_id, active)?;
            }
            return Ok(LoopControl::Continue);
        }

        if let Some(active) = active.as_mut() {
            loop {
                let ready_at = Instant::now();
                let step = active.framer.flush_rotation(ready_at)?;
                self.observe_framer_counters(&mut active.framer);
                let produced = step.output.is_some();
                if let Some(record) = step.output {
                    match self.append_record(file_id, active.base, record, ready_at)? {
                        AppendControl::Continue => {}
                        AppendControl::SealBefore => {
                            return self.seal_open_batch(event_tx, command_rx);
                        }
                        AppendControl::SealAfter => {
                            let remaining = if step.pending {
                                active.framer.flush_rotation(Instant::now())?
                            } else {
                                super::framing::FlushStep {
                                    output: None,
                                    pending: false,
                                }
                            };
                            if remaining.output.is_some() || remaining.pending {
                                // Seal rewind reconstructs any lookahead output
                                // from the first uncommitted source boundary.
                                return self.seal_open_batch(event_tx, command_rx);
                            }
                            return self.finalize_removed_file(file_id, event_tx, command_rx);
                        }
                    }
                }
                if !produced {
                    if step.pending
                        && let Some(start) = active.framer.pending_source_start()
                    {
                        let dropped = active
                            .framer
                            .next_expected_input_offset()
                            .checked_sub(start)
                            .ok_or(WorkerError::Inconsistent {
                                reason: "rotation pending range regressed",
                            })?;
                        self.telemetry
                            .add(WorkerCounter::PartialBytesDropped, dropped);
                        if let Some(suppressed) = self.health_event(HealthEventCategory::Partial) {
                            otel_warn!(
                                "filelog_receiver.rotation_partial_bytes_dropped",
                                dropped_bytes = dropped,
                                suppressed_events = suppressed
                            );
                        }
                    }
                    break;
                }
            }
        }

        self.finalize_removed_file(file_id, event_tx, command_rx)
    }

    fn rotation_finalization_due(
        &mut self,
        file_id: FileId,
        now: Instant,
    ) -> Result<bool, WorkerError> {
        let frontier = self.readers_ref()?.frontier(file_id)?;
        if frontier.present {
            let _ = self.rotation_waits.remove(&file_id);
            return Ok(false);
        }
        if let Some(wait) = self.rotation_waits.get(&file_id).copied() {
            debug_assert!(wait.stable_since <= wait.deadline);
            if now >= wait.deadline {
                return Ok(true);
            }
            self.readers_mut()?
                .cap_eof_deadline(file_id, wait.deadline)?;
            return Ok(false);
        }
        let deadline = now
            .checked_add(self.config.rotation.rotate_wait)
            .ok_or(WorkerError::RotationDeadlineOverflow { file_id })?;
        if self
            .rotation_waits
            .insert(
                file_id,
                RotationWait {
                    stable_since: now,
                    deadline,
                },
            )
            .is_some()
        {
            return Err(WorkerError::Inconsistent {
                reason: "rotation wait appeared during first EOF observation",
            });
        }
        self.readers_mut()?.cap_eof_deadline(file_id, deadline)?;
        Ok(false)
    }

    fn finalize_removed_file(
        &mut self,
        file_id: FileId,
        event_tx: &tokio_mpsc::Sender<WorkerEvent>,
        command_rx: &Receiver<WorkerCommand>,
    ) -> Result<LoopControl, WorkerError> {
        if self.cancellation_requested() {
            return Ok(LoopControl::Shutdown);
        }
        self.readers_ref()?.preflight_release_finalized(file_id)?;
        let durable = current_progress(&self.store, file_id)?;
        let frontier = self
            .open_batch
            .as_ref()
            .ok_or(WorkerError::MissingOpenBatch {
                operation: "reading a rotation finalization frontier",
            })?
            .progress_frontier(file_id)
            .unwrap_or(ProgressFrontier {
                file_epoch: durable.file_epoch,
                offset: durable.committed_offset,
                framing_resume: durable.framing_resume,
            });
        let last_seen_time_unix_nano = unix_nanos()?.1;
        let outcome = self
            .open_batch
            .as_mut()
            .ok_or(WorkerError::MissingOpenBatch {
                operation: "finalizing a removed file",
            })?
            .finalize_file(file_id, frontier, last_seen_time_unix_nano)?;
        if self.cancellation_requested() {
            return Ok(LoopControl::Shutdown);
        }
        match outcome {
            FinalizationOutcome::Merged => self.seal_open_batch(event_tx, command_rx),
            FinalizationOutcome::Direct(delta) => {
                if !self.commit_direct_progress(&delta)? {
                    return Ok(LoopControl::Shutdown);
                }
                Ok(LoopControl::Continue)
            }
        }
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
        let mut active = self.take_framer(file_id).ok_or(WorkerError::Inconsistent {
            reason: "due framer disappeared",
        })?;
        loop {
            let ready_at = Instant::now();
            let step = active.framer.poll_timeout(ready_at)?;
            self.observe_framer_counters(&mut active.framer);
            let produced = step.output.is_some();
            if let Some(record) = step.output {
                let control = self.append_record(file_id, active.base, record, ready_at)?;
                if control.requires_seal() {
                    return self.seal_open_batch(event_tx, command_rx).map(Some);
                }
            }
            if !produced {
                break;
            }
        }
        self.put_framer(file_id, active)?;
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
        let framed_telemetry = FramedTelemetry {
            decode_outcome: framed.decode_outcome,
            flush_reason: framed.flush_reason,
            truncated: framed.truncated,
            discarded_source_bytes: framed.discarded_source_bytes,
            split: framed.fragment.is_some(),
        };
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
                self.observe_framed_record(framed_telemetry);
                if let Some(reservation) = reservation {
                    let committed = self.record_numbers.commit(reservation)?;
                    if committed != record_number {
                        return Err(WorkerError::Inconsistent {
                            reason: "record-number commit changed the prepared projection",
                        });
                    }
                }

                Ok(if seal.is_some() {
                    AppendControl::SealAfter
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
                Ok(AppendControl::SealBefore)
            }
        }
    }

    fn observe_framer_error(&mut self, error: &FramerError) {
        if matches!(
            error,
            FramerError::Decode(DecodeError::FatalMalformed { .. })
        ) {
            self.telemetry.add(WorkerCounter::DecodeFailures, 1);
            if let Some(suppressed) = self.health_event(HealthEventCategory::Decode) {
                otel_warn!(
                    "filelog_receiver.decode_failed",
                    policy = "fail",
                    suppressed_events = suppressed
                );
            }
        }
    }

    fn observe_framer_counters(&mut self, framer: &mut Framer) {
        let fallback = framer.take_pattern_not_matched_count();
        self.telemetry.add(WorkerCounter::PatternFallback, fallback);
        if fallback != 0
            && let Some(suppressed) = self.health_event(HealthEventCategory::PatternFallback)
        {
            otel_info!(
                "filelog_receiver.multiline_pattern_fallback",
                fallback_lines = fallback,
                suppressed_events = suppressed
            );
        }
    }

    fn observe_framed_record(&mut self, framed: FramedTelemetry) {
        record_framed_telemetry(&self.telemetry, framed);
    }
}

fn record_framed_telemetry(telemetry: &WorkerTelemetryBridge, framed: FramedTelemetry) {
    match framed.decode_outcome {
        DecodeOutcome::Clean => {}
        DecodeOutcome::Replacements { count } => {
            telemetry.add(WorkerCounter::DecodeReplaceRecords, 1);
            telemetry.add(WorkerCounter::DecodeReplaceUnits, count);
        }
        DecodeOutcome::PreserveRaw { count } => {
            telemetry.add(WorkerCounter::DecodePreserveRawRecords, 1);
            telemetry.add(WorkerCounter::DecodePreserveRawUnits, count);
        }
    }
    if framed.truncated {
        telemetry.add(WorkerCounter::RecordsTruncated, 1);
    }
    telemetry.add(
        WorkerCounter::SourceBytesDiscarded,
        framed.discarded_source_bytes,
    );
    telemetry.add(WorkerCounter::SplitFragments, u64::from(framed.split));
    match framed.flush_reason {
        None => {}
        Some(FlushReason::MaxLines) => {
            telemetry.add(WorkerCounter::FlushMaxLines, 1);
        }
        Some(FlushReason::Timeout) => {
            telemetry.add(WorkerCounter::FlushTimeout, 1);
        }
        Some(FlushReason::OversizeLineBoundary) => {
            telemetry.add(WorkerCounter::FlushOversizeLineBoundary, 1);
        }
        Some(FlushReason::Rotation) => {
            telemetry.add(WorkerCounter::FlushRotation, 1);
        }
        Some(FlushReason::Drain) => {
            telemetry.add(WorkerCounter::FlushDrain, 1);
        }
    }
}

fn record_truncation_detection(telemetry: &WorkerTelemetryBridge) {
    telemetry.add(WorkerCounter::CopytruncateDetected, 1);
}

fn record_truncation_outcome(telemetry: &WorkerTelemetryBridge, policy: OnTruncate) {
    match policy {
        OnTruncate::Fail => {
            telemetry.add(WorkerCounter::CopytruncateFail, 1);
            telemetry.add(WorkerCounter::QuarantineTruncate, 1);
        }
        OnTruncate::ReadNew => {
            telemetry.add(WorkerCounter::CopytruncateReadNew, 1);
        }
    }
}

fn record_descriptor_unavailable_telemetry(telemetry: &WorkerTelemetryBridge) {
    telemetry.add(WorkerCounter::RotationDescriptorUnavailable, 1);
}

fn record_rotation_finalization_telemetry(telemetry: &WorkerTelemetryBridge) {
    telemetry.add(WorkerCounter::RotationFinalizations, 1);
}

fn record_checkpoint_maintenance_telemetry(
    telemetry: &WorkerTelemetryBridge,
    generation_before: u64,
    generation_after: u64,
    duration: Duration,
    cleaned_generations: usize,
    cleanup_failed: bool,
) {
    if generation_after != generation_before {
        telemetry.add(WorkerCounter::CheckpointCompactions, 1);
        telemetry.add(
            WorkerCounter::CheckpointCompactionDurationNs,
            duration_ns(duration),
        );
    }
    telemetry.add(
        WorkerCounter::CheckpointCleanupGenerations,
        u64::try_from(cleaned_generations).unwrap_or(u64::MAX),
    );
    telemetry.add(
        WorkerCounter::CheckpointCleanupFailures,
        u64::from(cleanup_failed),
    );
}

impl WorkerRuntime {
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
        self.clear_framers();
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
        self.readers_mut()?.prepare_batch_pause_order()?;
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
        self.publish_observations();
        match send_event_interruptibly(event_tx, command_rx, event)? {
            HandoffControl::Sent => Ok(LoopControl::Continue),
            HandoffControl::Drain => {
                self.drain_requested = true;
                Ok(LoopControl::Continue)
            }
            HandoffControl::Shutdown => Ok(LoopControl::Shutdown),
        }
    }

    fn commit_retained(&mut self, batch_id: u64, attempt: u32) -> Result<bool, WorkerError> {
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
        if !matches!(result, Ok(true)) {
            self.retained = Some(retained);
        }
        result
    }

    fn commit_logical_batch(&mut self, logical: &LogicalBatch) -> Result<bool, WorkerError> {
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
            if self.cancellation_requested() {
                return Ok(false);
            }
            if delta.finalize() {
                self.readers_ref()?
                    .preflight_release_finalized(delta.file_id())?;
            }
        }
        self.readers_mut()?.preflight_batch_commit()?;
        if self.cancellation_requested() {
            return Ok(false);
        }
        let _outcomes = self.store.commit_progress(updates)?;
        if logical.deltas().iter().any(|delta| delta.finalize()) {
            self.store.sync()?;
        }

        for delta in logical.deltas() {
            self.readers_mut()?.apply_preflighted_committed_progress(
                delta.file_id(),
                delta.expected_file_epoch(),
                delta.final_offset(),
                delta.final_framing_resume(),
            );
            self.readers_mut()?.install_committed_frontier_window(
                delta.file_id(),
                delta.final_window().cloned(),
            )?;
        }
        let resume = !self.drain_requested;
        self.readers_mut()?.finish_preflighted_batch_commit(resume);
        for delta in logical.deltas() {
            if delta.finalize() {
                let locator = self.readers_mut()?.release_finalized(delta.file_id())?;
                self.queue_finalization_feedback(locator)?;
                let _ = self.rotation_waits.remove(&delta.file_id());
                let _ = self.record_numbers.remove(delta.file_id());
                self.remove_drain_file(delta.file_id());
                record_rotation_finalization_telemetry(&self.telemetry);
            }
        }
        Ok(true)
    }

    fn drive_drain(
        &mut self,
        event_tx: &tokio_mpsc::Sender<WorkerEvent>,
        command_rx: &Receiver<WorkerCommand>,
    ) -> Result<LoopControl, WorkerError> {
        if self.cancellation_requested() {
            return Ok(LoopControl::Shutdown);
        }
        if self.retained.is_some() {
            return Ok(LoopControl::Continue);
        }
        if !self.drain_initialized {
            self.initialize_drain_replay()?;
        }

        while let Some(file_id) = self.drain_order.last().copied() {
            if self.cancellation_requested() {
                return Ok(LoopControl::Shutdown);
            }
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
                    .poll_until(Instant::now(), file_id, end_offset);
                self.publish_reader_gauges();
                if self.cancellation_requested() {
                    return Ok(LoopControl::Shutdown);
                }
                let poll = poll?;
                match poll {
                    ReaderPoll::Cancelled => return Ok(LoopControl::Shutdown),
                    ReaderPoll::Data(turn) => {
                        self.telemetry.add(
                            WorkerCounter::SourceBytesRead,
                            u64::try_from(turn.bytes().len()).unwrap_or(u64::MAX),
                        );
                        let control =
                            self.process_turn(turn, Instant::now(), event_tx, command_rx)?;
                        if control == LoopControl::Shutdown || self.retained.is_some() {
                            return Ok(control);
                        }
                        continue;
                    }
                    ReaderPoll::Truncated {
                        file_epoch,
                        committed_offset,
                        read_offset,
                        observed_size,
                        observed_fingerprint,
                        ..
                    } => {
                        let control = self.handle_truncation(
                            file_id,
                            file_epoch,
                            committed_offset,
                            read_offset,
                            observed_size,
                            observed_fingerprint,
                            event_tx,
                            command_rx,
                        )?;
                        if control == LoopControl::Shutdown || self.retained.is_some() {
                            return Ok(control);
                        }
                        continue;
                    }
                    ReaderPoll::EndOfFile { source_offset, .. } => {
                        return Err(WorkerError::DrainFrontierUnavailable {
                            file_id,
                            source_offset,
                            end_offset,
                        });
                    }
                    ReaderPoll::EvictionRequired(request) => {
                        self.discard_framer(request.victim_file_id);
                        self.readers_mut()?.confirm_eviction(request)?;
                        continue;
                    }
                    ReaderPoll::RemovedWithoutDescriptor { file_id } => {
                        let Some(locator) = self.contain_removed_without_descriptor(file_id)?
                        else {
                            return Ok(LoopControl::Shutdown);
                        };
                        self.remember_inactive_locator(locator, file_id)?;
                        continue;
                    }
                    ReaderPoll::EvidenceUnstable { next_probe, .. } => {
                        match command_rx.recv_timeout(self.next_wait(Some(next_probe))) {
                            Ok(command) => {
                                return self.handle_command(command, event_tx, command_rx);
                            }
                            Err(RecvTimeoutError::Timeout) => continue,
                            Err(RecvTimeoutError::Disconnected) => {
                                return Ok(LoopControl::Shutdown);
                            }
                        }
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
            let Some(mut active) = self.take_framer(file_id) else {
                let _ = self.drain_order.pop();
                let _ = self.drain_limits.remove(&file_id);
                continue;
            };
            loop {
                let ready_at = Instant::now();
                let step = active.framer.flush_drain(ready_at)?;
                self.observe_framer_counters(&mut active.framer);
                let produced = step.output.is_some();
                if let Some(record) = step.output {
                    let control = self.append_record(file_id, active.base, record, ready_at)?;
                    if control.requires_seal() {
                        return self.seal_open_batch(event_tx, command_rx);
                    }
                }
                if !produced {
                    // `pending` with no output is an unflushable tail under
                    // the configured disabled-partial policy. It is dropped
                    // only from memory and rewound to durable progress below.
                    break;
                }
            }
            self.telemetry
                .set(WorkerGauge::PartialBytesPending, self.partial_bytes_pending);
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
        self.telemetry.add(
            WorkerCounter::PartialBytesPendingDrain,
            self.partial_bytes_pending,
        );
        self.telemetry
            .set(WorkerGauge::PartialBytesPending, self.partial_bytes_pending);
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
        if self.cancellation_requested() {
            return Ok(());
        }
        let generation_before = self.store.generation();
        let cleanup_pending = !self.store.retired_generations().is_empty();
        let started = Instant::now();
        let mut cleaned_generations = 0usize;
        let mut cleanup_attempted = false;
        if self.cancellation_requested() {
            return Ok(());
        }
        let sync_result = self.store.sync_if_due();
        if self.cancellation_requested() {
            return Ok(());
        }
        let shutdown_requested = Arc::clone(&self.shutdown_requested);
        let result = match sync_result {
            Err(error) => Err(error),
            Ok(_) if cleanup_pending => {
                cleanup_attempted = true;
                self.store
                    .cleanup_retired_generations_cancellable(|| {
                        shutdown_requested.load(Ordering::Acquire)
                    })
                    .map(|cleaned| {
                        cleaned.map(|cleaned| {
                            cleaned_generations = cleaned;
                        })
                    })
            }
            Ok(_) => self
                .store
                .compact_if_due_cancellable(|| shutdown_requested.load(Ordering::Acquire))
                .map(|compacted| compacted.map(|_| ())),
        };
        let result = match result {
            Ok(Some(())) => Ok(()),
            Ok(None) => return Ok(()),
            Err(error) => Err(error),
        };
        if self.cancellation_requested() {
            return Ok(());
        }
        record_checkpoint_maintenance_telemetry(
            &self.telemetry,
            generation_before,
            self.store.generation(),
            started.elapsed(),
            cleaned_generations,
            result.is_err() && cleanup_attempted,
        );
        let _completed = self.observe_checkpoint_operation(result, "maintain checkpoint state")?;
        self.publish_store_observations();
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
                self.telemetry.add(WorkerCounter::CheckpointFailures, 1);
                self.checkpoint_maintenance_failures = self
                    .checkpoint_maintenance_failures
                    .checked_add(1)
                    .ok_or(WorkerError::Inconsistent {
                        reason: "checkpoint maintenance failure counter overflowed",
                    })?;
                if let Some(suppressed) =
                    self.health_event(HealthEventCategory::CheckpointMaintenance)
                {
                    otel_warn!(
                        "filelog_receiver.checkpoint_operation_failed",
                        operation = operation,
                        consecutive_failures = u64::from(self.checkpoint_maintenance_failures),
                        suppressed_events = suppressed
                    );
                }
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

    fn retry_direct_checkpoint_operation<T>(
        &mut self,
        operation: &'static str,
        mut attempt: impl FnMut(&mut Self) -> Result<T, WorkerError>,
    ) -> Result<Option<T>, WorkerError> {
        loop {
            if self.cancellation_requested() {
                return Ok(None);
            }
            match attempt(self) {
                Ok(value) => {
                    self.checkpoint_maintenance_failures = 0;
                    self.maintenance_retry_pending = false;
                    return Ok(Some(value));
                }
                Err(error)
                    if matches!(
                        &error,
                        WorkerError::Store(_) | WorkerError::Identity(IdentityError::Store(_))
                    ) =>
                {
                    self.telemetry.add(WorkerCounter::CheckpointFailures, 1);
                    self.checkpoint_maintenance_failures = self
                        .checkpoint_maintenance_failures
                        .checked_add(1)
                        .ok_or(WorkerError::Inconsistent {
                            reason: "checkpoint operation failure counter overflowed",
                        })?;
                    if let Some(suppressed) =
                        self.health_event(HealthEventCategory::CheckpointMaintenance)
                    {
                        otel_warn!(
                            "filelog_receiver.checkpoint_operation_failed",
                            operation = operation,
                            consecutive_failures = u64::from(self.checkpoint_maintenance_failures),
                            suppressed_events = suppressed
                        );
                    }
                    if self.checkpoint_maintenance_failures
                        >= self.config.checkpoint.max_consecutive_failures
                    {
                        return Err(error);
                    }
                    std::thread::yield_now();
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn publish_observations(&mut self) {
        let stats = self.store.stats();
        self.publish_store_deltas(&stats);
        self.telemetry
            .set(WorkerGauge::CheckpointWalSize, stats.wal_bytes);
        self.telemetry.set(
            WorkerGauge::FilesTracked,
            u64::try_from(stats.records).unwrap_or(u64::MAX),
        );
        self.telemetry.set(
            WorkerGauge::FilesQuarantined,
            u64::try_from(stats.quarantined_records).unwrap_or(u64::MAX),
        );
        self.telemetry.set(
            WorkerGauge::TrackedSaturated,
            u64::from(stats.records >= self.config.limits.max_tracked_files as usize),
        );

        self.publish_reader_gauges();

        self.telemetry
            .set(WorkerGauge::PartialBytesPending, self.partial_bytes_pending);
    }

    /// Refreshes reader populations from constant-state counters and bounded
    /// scheduling-index lengths; it never scans the reader table.
    fn publish_reader_gauges(&self) {
        if let Some(readers) = self.readers.as_ref() {
            let reader_stats = readers.stats();
            self.telemetry.set(
                WorkerGauge::FilesOpen,
                u64::try_from(reader_stats.open_files).unwrap_or(u64::MAX),
            );
            self.telemetry.set(
                WorkerGauge::FilesDescriptorBlocked,
                u64::try_from(reader_stats.descriptor_blocked_readers).unwrap_or(u64::MAX),
            );
            self.telemetry.set(
                WorkerGauge::FilesRemovedWaiting,
                u64::try_from(reader_stats.removed_readers).unwrap_or(u64::MAX),
            );
            self.telemetry.set(
                WorkerGauge::DescriptorSaturated,
                u64::from(reader_stats.descriptor_blocked_readers != 0),
            );
        } else {
            self.telemetry.set(WorkerGauge::FilesOpen, 0);
            self.telemetry.set(WorkerGauge::FilesDescriptorBlocked, 0);
            self.telemetry.set(WorkerGauge::FilesRemovedWaiting, 0);
            self.telemetry.set(WorkerGauge::DescriptorSaturated, 0);
        }
    }

    fn publish_store_observations(&mut self) {
        let stats = self.store.stats();
        self.publish_store_deltas(&stats);
        self.telemetry
            .set(WorkerGauge::CheckpointWalSize, stats.wal_bytes);
    }

    fn publish_store_deltas(&mut self, stats: &StoreStats) {
        macro_rules! delta {
            ($field:ident, $counter:ident) => {
                self.telemetry.add(
                    WorkerCounter::$counter,
                    stats.$field.saturating_sub(self.observed_store.$field),
                );
                self.observed_store.$field = stats.$field;
            };
        }
        delta!(wal_bytes_appended, CheckpointWalBytes);
        delta!(transactions_appended, CheckpointTransactions);
        delta!(syncs, CheckpointSyncs);
        delta!(persist_duration_ns, CheckpointPersistDurationNs);
        delta!(persist_operations, CheckpointPersistOperations);
        delta!(sync_duration_ns, CheckpointSyncDurationNs);
        delta!(sync_operations, CheckpointSyncOperations);
        delta!(quarantine_reset_beginning, QuarantineResetBeginning);
        delta!(quarantine_reset_end, QuarantineResetEnd);
        delta!(quarantine_keep_failed, QuarantineKeepFailed);
        delta!(quarantine_removals, QuarantineRemove);
        if !self.observed_store.namespace_lock_observed {
            self.telemetry.add(
                WorkerCounter::NamespaceLockWaitNs,
                stats.namespace_lock_wait_ns,
            );
            self.telemetry.add(WorkerCounter::NamespaceLockWaits, 1);
            self.telemetry.add(
                WorkerCounter::NamespaceLockContentions,
                stats.namespace_lock_contentions,
            );
            self.observed_store.namespace_lock_wait_ns = stats.namespace_lock_wait_ns;
            self.observed_store.namespace_lock_contentions = stats.namespace_lock_contentions;
            self.observed_store.namespace_lock_observed = true;
        }
    }

    fn health_event(&mut self, category: HealthEventCategory) -> Option<u64> {
        match self.health_events.admit(category, Instant::now()) {
            Some(suppressed) => {
                if suppressed != 0 {
                    emit_suppression_summary(category, suppressed);
                }
                Some(suppressed)
            }
            None => {
                self.telemetry.add(WorkerCounter::HealthSuppressed, 1);
                None
            }
        }
    }

    fn next_wait(&self, reader_deadline: Option<Instant>) -> Duration {
        let now = Instant::now();
        let mut wait = COMMAND_POLL_INTERVAL;
        for deadline in [
            reader_deadline,
            self.pending_reconciliation_retry_at,
            self.store.next_sync_deadline(),
            self.open_batch.as_ref().and_then(OpenBatch::deadline),
            self.framers
                .values()
                .filter_map(|active| active.framer.deadline())
                .min(),
            self.rotation_waits.values().map(|wait| wait.deadline).min(),
        ]
        .into_iter()
        .flatten()
        {
            wait = wait.min(deadline.saturating_duration_since(now));
        }
        wait
    }

    fn next_maintenance_wait(&self) -> Duration {
        let now = Instant::now();
        self.store
            .next_sync_deadline()
            .map_or(COMMAND_POLL_INTERVAL, |deadline| {
                COMMAND_POLL_INTERVAL.min(deadline.saturating_duration_since(now))
            })
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

    fn take_framer(&mut self, file_id: FileId) -> Option<ActiveFramer> {
        let active = self.framers.remove(&file_id)?;
        self.partial_bytes_pending = self
            .partial_bytes_pending
            .saturating_sub(active.pending_bytes);
        Some(active)
    }

    fn put_framer(&mut self, file_id: FileId, mut active: ActiveFramer) -> Result<(), WorkerError> {
        if self.framers.contains_key(&file_id) {
            return Err(WorkerError::Inconsistent {
                reason: "framer reappeared while caller owned it",
            });
        }
        active.pending_bytes = active.framer.pending_source_start().map_or(0, |start| {
            active
                .framer
                .next_expected_input_offset()
                .saturating_sub(start)
        });
        self.partial_bytes_pending = self
            .partial_bytes_pending
            .saturating_add(active.pending_bytes);
        self.telemetry
            .set(WorkerGauge::PartialBytesPending, self.partial_bytes_pending);
        let previous = self.framers.insert(file_id, active);
        debug_assert!(previous.is_none());
        Ok(())
    }

    fn discard_framer(&mut self, file_id: FileId) {
        let _ = self.take_framer(file_id);
        self.telemetry
            .set(WorkerGauge::PartialBytesPending, self.partial_bytes_pending);
    }

    fn clear_framers(&mut self) {
        self.framers.clear();
        self.partial_bytes_pending = 0;
        self.telemetry.set(WorkerGauge::PartialBytesPending, 0);
    }

    fn shutdown_resources(&mut self) -> Result<(), WorkerError> {
        let mut first_error = None;
        if let Some(discovery) = self.discovery.take() {
            discovery.request_shutdown();
            let join = discovery.into_join_handle();
            if join.is_finished() && join.join().is_err() {
                first_error = Some(WorkerError::DiscoveryThreadPanicked);
            }
        }
        self.telemetry.set(WorkerGauge::FilesPending, 0);
        self.telemetry.set(WorkerGauge::CandidateOldestAgeNs, 0);
        self.telemetry
            .set(WorkerGauge::CandidateOverflowPersistenceNs, 0);
        if self.store.has_pending_wal_append() {
            if let Err(error) = self.store.drain()
                && first_error.is_none()
            {
                first_error = Some(WorkerError::Store(error));
            }
        } else {
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
    ) -> Result<bool, WorkerError> {
        if delta.finalize() {
            self.readers_ref()?
                .preflight_release_finalized(delta.file_id())?;
        }
        if self.cancellation_requested() {
            return Ok(false);
        }
        let Some(()) = self
            .retry_direct_checkpoint_operation("persist direct progress", |runtime| {
                persist_direct_progress(&mut runtime.store, delta)
            })?
        else {
            return Ok(false);
        };
        self.readers_mut()?.observe_committed_progress(
            delta.file_id(),
            delta.expected_file_epoch(),
            delta.final_offset(),
            delta.final_framing_resume(),
        )?;
        if delta.finalize() {
            let locator = self.readers_mut()?.release_finalized(delta.file_id())?;
            self.queue_finalization_feedback(locator)?;
            let _ = self.rotation_waits.remove(&delta.file_id());
            let _ = self.record_numbers.remove(delta.file_id());
            self.remove_drain_file(delta.file_id());
            record_rotation_finalization_telemetry(&self.telemetry);
        }
        self.publish_observations();
        Ok(true)
    }
}

fn worker_batch(retained: &RetainedBatch) -> WorkerBatch {
    WorkerBatch {
        batch_id: retained.batch_id,
        attempt: retained.attempt,
        records: retained.logical.outbound_records(),
        record_count: retained.logical.record_count(),
        logical_bytes: retained.logical.logical_bytes(),
        source_bytes: retained.logical.source_bytes(),
    }
}

fn emit_suppression_summary(category: HealthEventCategory, suppressed: u64) {
    otel_info!(
        "filelog_receiver.health_events_suppressed",
        category = category.as_str(),
        suppressed_events = suppressed
    );
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
        revoked: reserved_vec(capacity, "discovery revocation feedback")?,
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
        committed_frontier_guard: record.committed_frontier_guard,
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
    if delta.finalize() {
        store.sync()?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "worker/tests.rs"]
mod tests;
