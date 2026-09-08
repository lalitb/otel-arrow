// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Transactional startup, bounded shard handoff, and joined shutdown.

use std::{
    collections::BTreeMap,
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, TryLockError,
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel},
    },
    thread::JoinHandle,
    time::{Duration, Instant, SystemTime},
};

use crate::{
    AffinityMode, AggregationWindow, DropReason, EventBatch, EventSource, Limits,
    ObjectSymbolResolver, Platform, PlatformGuard, ProcessProvider, ProcfsProcessProvider,
    ProfileSnapshot, ProfilerConfig, ProfilerError, ProfilerStatistics, Result, ShardPlan,
    ShardPlanner, SymbolResolver, ValidatedConfig, error::bounded_message,
    finalize::merge_snapshots,
};

static PROFILER_LEASE: AtomicBool = AtomicBool::new(false);
const WAITING: u8 = 0;
const RUNNING: u8 = 1;
const DRAINING: u8 = 2;
const POLL: Duration = Duration::from_millis(20);
const STARTUP_WAIT: Duration = Duration::from_secs(5);

/// Injectable reporting clock. Shutdown deadlines always use real `Instant`s.
pub trait Clock: Send + Sync {
    /// Current monotonic reporting time.
    fn monotonic(&self) -> Instant;
    /// Wall time captured once as the origin for reporting windows.
    fn wall_time(&self) -> SystemTime;
    /// Current time in the event clock's BOOTTIME domain, when available.
    fn event_time_ns(&self) -> Result<Option<u64>> {
        Ok(None)
    }
}

/// System reporting clock.
#[derive(Debug, Default)]
pub struct SystemClock;
impl Clock for SystemClock {
    fn monotonic(&self) -> Instant {
        Instant::now()
    }
    fn wall_time(&self) -> SystemTime {
        SystemTime::now()
    }
    fn event_time_ns(&self) -> Result<Option<u64>> {
        native_event_time()
    }
}

/// Injectable worker-affinity operation. It must return promptly.
pub trait WorkerAffinity: Send + Sync {
    /// Pins the current thread or returns a typed failure.
    fn pin(&self, cpu: u32) -> Result<()>;
}

#[derive(Debug)]
struct SystemAffinity;
impl WorkerAffinity for SystemAffinity {
    fn pin(&self, cpu: u32) -> Result<()> {
        if core_affinity::set_for_current(core_affinity::CoreId { id: cpu as usize }) {
            Ok(())
        } else {
            Err(ProfilerError::WorkerStartup {
                shard: usize::MAX,
                reason: format!("cannot pin to CPU {cpu}"),
            })
        }
    }
}

/// First terminal worker/control-plane failure, with bounded detail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeFailure {
    /// Shard ID or `usize::MAX` for the coordinator/finalizer.
    pub shard: usize,
    /// Diagnostic bounded by `max_metadata_string_bytes`.
    pub reason: String,
}

/// Bounded receiver of owned completed windows.
pub struct SnapshotReceiver {
    receiver: Receiver<ProfileSnapshot>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for SnapshotReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotReceiver").finish_non_exhaustive()
    }
}

impl SnapshotReceiver {
    /// Waits no longer than `timeout` for ownership of one window.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<ProfileSnapshot> {
        match self.receiver.recv_timeout(timeout) {
            Ok(snapshot) => Ok(snapshot),
            Err(RecvTimeoutError::Timeout) => Err(self
                .shared
                .failure_error()
                .unwrap_or(ProfilerError::SnapshotReceiveTimeout)),
            Err(RecvTimeoutError::Disconnected) => Err(self
                .shared
                .failure_error()
                .unwrap_or(ProfilerError::SnapshotConsumerDisconnected)),
        }
    }

    /// Takes a window without blocking.
    pub fn try_recv(&self) -> Result<Option<ProfileSnapshot>> {
        match self.receiver.try_recv() {
            Ok(snapshot) => Ok(Some(snapshot)),
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if let Some(error) = self.shared.failure_error() {
                    Err(error)
                } else {
                    Ok(None)
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(self
                .shared
                .failure_error()
                .unwrap_or(ProfilerError::SnapshotConsumerDisconnected)),
        }
    }
}

/// Completed cleanup and final disjoint sample accounting.
#[derive(Clone, Debug)]
pub struct ShutdownReport {
    /// Every owned worker, buffer, link, and map was released.
    pub cleanup_complete: bool,
    /// Total joined shard workers, retained across shutdown retries.
    pub workers_joined: usize,
    /// Counts for all completed and dropped windows plus kernel counters.
    pub statistics: ProfilerStatistics,
    /// Collection can fail even when resource cleanup succeeds.
    pub failure: Option<RuntimeFailure>,
}

/// Runtime-neutral construction with injectable platform, metadata, and clock.
pub struct ProfilerBuilder {
    config: ProfilerConfig,
    platform: Arc<dyn Platform>,
    process_provider: Option<Arc<dyn ProcessProvider>>,
    clock: Arc<dyn Clock>,
    affinity: Arc<dyn WorkerAffinity>,
}

impl std::fmt::Debug for ProfilerBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfilerBuilder")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ProfilerBuilder {
    /// Uses the native Linux backend, or a typed unsupported-platform backend.
    #[must_use]
    pub fn new(config: ProfilerConfig) -> Self {
        Self {
            config,
            platform: native_platform(),
            process_provider: None,
            clock: Arc::new(SystemClock),
            affinity: Arc::new(SystemAffinity),
        }
    }

    /// Supplies a bounded platform implementation (also used by non-root tests).
    #[must_use]
    pub fn with_platform(mut self, platform: Arc<dyn Platform>) -> Self {
        self.platform = platform;
        self
    }

    /// Supplies metadata access. Injected implementations must respect the
    /// configured row/string bounds and complete each lookup in bounded time.
    #[must_use]
    pub fn with_process_provider(mut self, provider: Arc<dyn ProcessProvider>) -> Self {
        self.process_provider = Some(provider);
        self
    }

    /// Supplies a clock for deterministic reporting-window tests.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Supplies affinity behavior without exposing a kernel-library type.
    #[must_use]
    pub fn with_affinity(mut self, affinity: Arc<dyn WorkerAffinity>) -> Self {
        self.affinity = affinity;
        self
    }

    /// Starts transactionally. Sampling starts only after every worker has
    /// pinned (if requested), allocated bounded state, and signaled readiness.
    pub fn start(self) -> Result<(Profiler, SnapshotReceiver)> {
        let config = Arc::new(self.config.validate()?);
        let lease = SingletonLease::acquire()?;
        let topology = self.platform.discover_topology(&config)?;
        let shards = ShardPlanner::plan(&topology, &config)?;
        let quotas = per_shard_limits(&config.get().limits, shards.len())?;
        let prepared = self.platform.prepare(&config, &shards)?;
        let shared = Arc::new(Shared {
            phase: AtomicU8::new(WAITING),
            deadline: Mutex::new(None),
            gate: (Mutex::new(()), Condvar::new()),
            statistics: Mutex::new(ProfilerStatistics::default()),
            failure: Mutex::new(None),
            guard: Mutex::new(prepared.guard),
            ingress_stopped: AtomicBool::new(false),
            sampling_started: AtomicBool::new(false),
            pool: Arc::new(WindowPool {
                used: AtomicUsize::new(0),
                maximum: config.get().limits.max_pending_shard_windows,
            }),
            max_detail: config.get().limits.max_metadata_string_bytes,
        });
        // Runtime is the rollback guard from this point onward. Its Drop joins
        // all spawned threads before releasing the process-local lease.
        let mut runtime = Runtime {
            shared: Arc::clone(&shared),
            workers: Vec::new(),
            finalizer: None,
            sender: None,
            lease: Some(lease),
            workers_joined: 0,
            kernel_accounted: false,
            cleaned: false,
        };
        if prepared.sources.len() != shards.len() {
            return Err(ProfilerError::PartialStartup(
                "platform source count differs from shard plan".to_owned(),
            ));
        }
        let (sender, receiver) = sync_channel(config.get().limits.max_pending_shard_windows);
        let (snapshot_sender, snapshot_receiver) =
            sync_channel(config.get().limits.max_pending_snapshots);
        runtime.sender = Some(sender.clone());
        let finalizer_shared = Arc::clone(&shared);
        let finalizer_config = Arc::clone(&config);
        let shard_count = shards.len();
        runtime.finalizer = Some(
            std::thread::Builder::new()
                .name("ebpf-profiler-finalizer".to_owned())
                .stack_size(config.get().limits.worker_stack_bytes)
                .spawn(move || {
                    guarded_thread(usize::MAX, &finalizer_shared, || {
                        finalizer_loop(
                            receiver,
                            snapshot_sender,
                            shard_count,
                            &finalizer_config,
                            &finalizer_shared,
                        )
                    })
                })
                .map_err(|error| ProfilerError::WorkerStartup {
                    shard: usize::MAX,
                    reason: error.to_string(),
                })?,
        );

        let (ready_sender, ready_receiver) = sync_channel(shard_count);
        let origin = TimeOrigin {
            monotonic: self.clock.monotonic(),
            wall: self.clock.wall_time(),
        };
        for ((plan, source), limits) in shards.into_iter().zip(prepared.sources).zip(quotas) {
            let provider = self.process_provider.as_ref().map_or_else(
                || {
                    Arc::new(ProcfsProcessProvider::new(
                        config.get().process.procfs_root.clone(),
                        limits.max_mappings,
                        limits.max_procfs_file_bytes,
                        config.get().process.collect_thread_names,
                        config.get().process.collect_mappings,
                        limits.max_metadata_string_bytes,
                    )) as Arc<dyn ProcessProvider>
                },
                Arc::clone,
            );
            let shard = plan.id;
            let context = WorkerContext {
                plan,
                source,
                config: Arc::clone(&config),
                limits,
                provider,
                clock: Arc::clone(&self.clock),
                affinity: Arc::clone(&self.affinity),
                shared: Arc::clone(&shared),
                sender: sender.clone(),
                ready: Some(ready_sender.clone()),
                origin,
            };
            let worker_shared = Arc::clone(&shared);
            let handle = std::thread::Builder::new()
                .name(format!("ebpf-profiler-shard-{shard}"))
                .stack_size(config.get().limits.worker_stack_bytes)
                .spawn(move || guarded_thread(shard, &worker_shared, || worker_loop(context)))
                .map_err(|error| ProfilerError::WorkerStartup {
                    shard,
                    reason: error.to_string(),
                })?;
            runtime.workers.push(handle);
        }
        drop(sender);
        drop(ready_sender);
        let startup_deadline = Instant::now()
            .checked_add(STARTUP_WAIT)
            .ok_or_else(|| ProfilerError::invalid("startup", "deadline overflow"))?;
        for _ in 0..shard_count {
            match ready_receiver
                .recv_timeout(startup_deadline.saturating_duration_since(Instant::now()))
            {
                Ok(()) => {}
                Err(error) => {
                    return Err(shared.failure_error().unwrap_or_else(|| {
                        ProfilerError::PartialStartup(format!("worker readiness failed: {error}"))
                    }));
                }
            }
        }
        if let Some(error) = shared.failure_error() {
            return Err(error);
        }
        let coverage = lock(&shared.guard)?.start_sampling()?;
        shared.update(|stats| {
            stats.attached_cpus = coverage.attached_cpus as u64;
            stats.unavailable_cpus = coverage.unavailable_cpus as u64;
        })?;
        shared.sampling_started.store(true, Ordering::Release);
        shared.phase.store(RUNNING, Ordering::Release);
        shared.gate.1.notify_all();
        Ok((
            Profiler {
                state: Mutex::new(ProfilerState::Running(runtime)),
                shared: Arc::clone(&shared),
            },
            SnapshotReceiver {
                receiver: snapshot_receiver,
                shared,
            },
        ))
    }
}

/// Running profiler. Explicit shutdown bounds caller waiting; Drop cancels and
/// joins as a safety net and never detaches a live worker or releases its lease.
pub struct Profiler {
    state: Mutex<ProfilerState>,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Profiler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Profiler").finish_non_exhaustive()
    }
}

impl Profiler {
    /// Counts from finalized/dropped windows; kernel counters become final at
    /// shutdown. No per-frame global locking is used to expose these counts.
    pub fn statistics(&self) -> Result<ProfilerStatistics> {
        Ok(lock(&self.shared.statistics)?.clone())
    }

    /// First terminal failure, if collection or cleanup failed.
    #[must_use]
    pub fn failure(&self) -> Option<RuntimeFailure> {
        self.shared.failure()
    }

    /// Disables ingress and asks workers to drain without waiting for joins.
    /// Call `shutdown` to receive the cleanup report.
    pub fn cancel(&self) -> Result<()> {
        self.shared.stop_ingress()?;
        self.shared.request_stop(Instant::now())
    }

    /// Detaches ingress and waits until `deadline` for bounded drain,
    /// finalization, and joined cleanup. Timeout preserves ownership for retry.
    pub fn shutdown(&self, deadline: Instant) -> Result<ShutdownReport> {
        let mut state = lock_until(&self.state, deadline)?;
        if let ProfilerState::Stopped(report) = &*state {
            return Ok((**report).clone());
        }
        let ProfilerState::Running(runtime) = &mut *state else {
            return Err(ProfilerError::InternalInvariant(
                "invalid profiler state".to_owned(),
            ));
        };
        match runtime.finish(Some(deadline)) {
            Ok(report) => {
                *state = ProfilerState::Stopped(Box::new(report.clone()));
                Ok(report)
            }
            Err(ProfilerError::ShutdownTimeout) => {
                self.shared.update(|stats| {
                    stats.shutdown_timeouts = stats.shutdown_timeouts.saturating_add(1)
                })?;
                Err(ProfilerError::ShutdownTimeout)
            }
            Err(error) => Err(error),
        }
    }
}

enum ProfilerState {
    Running(Runtime),
    Stopped(Box<ShutdownReport>),
}

struct Runtime {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<Result<()>>>,
    finalizer: Option<JoinHandle<Result<()>>>,
    sender: Option<SyncSender<ShardWindow>>,
    lease: Option<SingletonLease>,
    workers_joined: usize,
    kernel_accounted: bool,
    cleaned: bool,
}

impl Runtime {
    fn finish(&mut self, deadline: Option<Instant>) -> Result<ShutdownReport> {
        if let Err(error) = self.shared.stop_ingress() {
            self.shared.remember_failure(usize::MAX, &error);
        }
        self.shared
            .request_stop(deadline.unwrap_or_else(Instant::now))?;
        wait_for(&self.workers, deadline)?;
        for worker in self.workers.drain(..) {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => self.shared.remember_failure(usize::MAX, &error),
                Err(_) => self.shared.remember_failure(
                    usize::MAX,
                    &ProfilerError::InternalInvariant("worker panicked".to_owned()),
                ),
            }
            self.workers_joined += 1;
        }
        drop(self.sender.take());
        if let Some(handle) = self.finalizer.as_ref() {
            wait_for(std::slice::from_ref(handle), deadline)?;
        }
        if let Some(handle) = self.finalizer.take() {
            match handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => self.shared.remember_failure(usize::MAX, &error),
                Err(_) => self.shared.remember_failure(
                    usize::MAX,
                    &ProfilerError::InternalInvariant("finalizer panicked".to_owned()),
                ),
            }
        }
        if !self.kernel_accounted {
            match lock(&self.shared.guard)?.statistics() {
                Ok(kernel) => self.shared.update(|stats| {
                    if kernel.sampling_periods_attempted > 0 {
                        stats.unobserved_samples = kernel
                            .sampling_periods_attempted
                            .checked_sub(kernel.idle_samples_skipped)
                            .and_then(|produced| produced.checked_sub(stats.raw_events_received));
                    }
                    stats.merge(&kernel);
                })?,
                Err(error) => self.shared.remember_failure(usize::MAX, &error),
            }
            self.kernel_accounted = true;
        }
        let mut guard = lock(&self.shared.guard)?;
        let cleanup = guard.cleanup();
        // A receiver may outlive the profiler through Shared. Do not retain
        // kernel resources through that Arc, even if explicit cleanup failed.
        let released = std::mem::replace(&mut *guard, Box::new(ReleasedGuard));
        drop(guard);
        drop(released);
        if let Err(error) = &cleanup {
            self.shared.remember_failure(usize::MAX, error);
        }
        self.cleaned = true;
        drop(self.lease.take());
        Ok(ShutdownReport {
            cleanup_complete: cleanup.is_ok(),
            workers_joined: self.workers_joined,
            statistics: lock(&self.shared.statistics)?.clone(),
            failure: self.shared.failure(),
        })
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if !self.cleaned {
            // Native callbacks have a bounded contract. Unlike timed public
            // shutdown, Drop waits rather than leaking a worker and its lease.
            if let Err(error) = self.finish(None) {
                self.shared.remember_failure(usize::MAX, &error);
            }
        }
    }
}

struct ReleasedGuard;
impl PlatformGuard for ReleasedGuard {
    fn stop_sampling(&mut self) -> Result<()> {
        Ok(())
    }
    fn cleanup(&mut self) -> Result<()> {
        Ok(())
    }
}

struct Shared {
    phase: AtomicU8,
    deadline: Mutex<Option<Instant>>,
    gate: (Mutex<()>, Condvar),
    statistics: Mutex<ProfilerStatistics>,
    failure: Mutex<Option<RuntimeFailure>>,
    guard: Mutex<Box<dyn PlatformGuard>>,
    ingress_stopped: AtomicBool,
    sampling_started: AtomicBool,
    pool: Arc<WindowPool>,
    max_detail: usize,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut ProfilerStatistics)) -> Result<()> {
        f(&mut *lock(&self.statistics)?);
        Ok(())
    }

    fn failure(&self) -> Option<RuntimeFailure> {
        match self.failure.lock() {
            Ok(failure) => failure.clone(),
            Err(_) => Some(RuntimeFailure {
                shard: usize::MAX,
                reason: "profiler failure lock poisoned".to_owned(),
            }),
        }
    }

    fn failure_error(&self) -> Option<ProfilerError> {
        self.failure().map(|failure| ProfilerError::WorkerFailed {
            shard: failure.shard,
            reason: failure.reason,
        })
    }

    fn remember_failure(&self, shard: usize, error: &ProfilerError) {
        if let Ok(mut failure) = self.failure.lock()
            && failure.is_none()
        {
            *failure = Some(RuntimeFailure {
                shard,
                reason: bounded_message(error, self.max_detail),
            });
        }
    }

    fn stop_ingress(&self) -> Result<()> {
        if !self.ingress_stopped.swap(true, Ordering::AcqRel) {
            lock(&self.guard)?.stop_sampling()?;
        }
        Ok(())
    }

    fn request_stop(&self, deadline: Instant) -> Result<()> {
        let mut stored = lock(&self.deadline)?;
        *stored = Some(stored.map_or(deadline, |current| current.min(deadline)));
        self.phase.store(DRAINING, Ordering::Release);
        self.gate.1.notify_all();
        Ok(())
    }

    fn expired(&self) -> Result<bool> {
        Ok(lock(&self.deadline)?.is_some_and(|deadline| Instant::now() >= deadline))
    }

    fn fail(&self, shard: usize, error: &ProfilerError) {
        self.remember_failure(shard, error);
        if let Err(cleanup) = self.stop_ingress() {
            self.remember_failure(usize::MAX, &cleanup);
        }
        if let Err(stopping) = self.request_stop(Instant::now()) {
            self.remember_failure(usize::MAX, &stopping);
        }
    }
}

fn guarded_thread(shard: usize, shared: &Shared, run: impl FnOnce() -> Result<()>) -> Result<()> {
    // A callback panic is a terminal runtime fault, never success or a detached
    // drain thread. Ordinary environmental failures remain typed Results.
    let result = catch_unwind(AssertUnwindSafe(run)).unwrap_or_else(|_| {
        Err(ProfilerError::WorkerFailed {
            shard,
            reason: "worker callback panicked".to_owned(),
        })
    });
    if let Err(error) = &result {
        shared.fail(shard, error);
    }
    result
}

#[derive(Debug)]
struct SingletonLease;
impl SingletonLease {
    fn acquire() -> Result<Self> {
        let _old = PROFILER_LEASE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| ProfilerError::AlreadyRunning)?;
        Ok(Self)
    }
}
impl Drop for SingletonLease {
    fn drop(&mut self) {
        PROFILER_LEASE.store(false, Ordering::Release);
    }
}

struct WindowPool {
    used: AtomicUsize,
    maximum: usize,
}
struct WindowPermit {
    pool: Arc<WindowPool>,
}
impl WindowPool {
    fn acquire(self: &Arc<Self>) -> Option<WindowPermit> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used < self.maximum).then_some(used + 1)
            })
            .ok()
            .map(|_| WindowPermit {
                pool: Arc::clone(self),
            })
    }
}
impl Drop for WindowPermit {
    fn drop(&mut self) {
        let _old = self.pool.used.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy)]
struct TimeOrigin {
    monotonic: Instant,
    wall: SystemTime,
}
impl TimeOrigin {
    fn wall_at(&self, elapsed: Duration) -> Result<SystemTime> {
        self.wall
            .checked_add(elapsed)
            .ok_or_else(|| ProfilerError::InternalInvariant("reporting time overflow".to_owned()))
    }
}

struct WorkerContext {
    plan: ShardPlan,
    source: Box<dyn EventSource>,
    config: Arc<ValidatedConfig>,
    limits: Limits,
    provider: Arc<dyn ProcessProvider>,
    clock: Arc<dyn Clock>,
    affinity: Arc<dyn WorkerAffinity>,
    shared: Arc<Shared>,
    sender: SyncSender<ShardWindow>,
    ready: Option<SyncSender<()>>,
    origin: TimeOrigin,
}

struct ShardWindow {
    sequence: u64,
    shard: usize,
    end: SystemTime,
    window: AggregationWindow,
    _permit: WindowPermit,
}

fn new_window(context: &WorkerContext, start: SystemTime) -> Result<AggregationWindow> {
    let symbols: Box<dyn SymbolResolver> = if context.config.get().symbolization.enabled {
        Box::new(ObjectSymbolResolver::with_limits(
            context.limits.max_symbols,
            context.limits.max_processes,
            context.limits.max_symbol_file_bytes,
            context.limits.max_metadata_string_bytes,
        )?)
    } else {
        Box::new(DisabledSymbols)
    };
    AggregationWindow::new(
        context.limits.clone(),
        Duration::from_nanos(
            1_000_000_000 / u64::from(context.config.get().samples_per_second.get()),
        ),
        start,
        Arc::clone(&context.provider),
        symbols,
    )
}

fn worker_loop(mut context: WorkerContext) -> Result<()> {
    if context.config.get().affinity != AffinityMode::Disabled
        && let Err(error) = context.affinity.pin(context.plan.affinity_cpu)
    {
        if context.config.get().affinity == AffinityMode::Required {
            return Err(error);
        }
        context
            .shared
            .update(|stats| stats.affinity_failures = stats.affinity_failures.saturating_add(1))?;
    }
    let mut window = new_window(&context, context.origin.wall)?;
    let mut batch = EventBatch::default();
    batch
        .samples
        .try_reserve_exact(context.limits.max_raw_events_queued)
        .map_err(|_| ProfilerError::Allocation("worker raw batch"))?;
    let ready = context.ready.take().ok_or_else(|| {
        ProfilerError::InternalInvariant("worker readiness sender missing".to_owned())
    })?;
    ready
        .send(())
        .map_err(|_| ProfilerError::PartialStartup("readiness receiver closed".to_owned()))?;
    drop(ready);
    while context.shared.phase.load(Ordering::Acquire) == WAITING {
        let guard = lock(&context.shared.gate.0)?;
        let _wait = context
            .shared
            .gate
            .1
            .wait_timeout(guard, POLL)
            .map_err(|_| ProfilerError::InternalInvariant("startup gate poisoned".to_owned()))?;
    }
    if !context.shared.sampling_started.load(Ordering::Acquire) {
        return Ok(());
    }
    let interval = context.config.get().reporting_interval;
    let mut sequence = 0u64;
    let mut start = context.origin.wall;
    loop {
        let draining = context.shared.phase.load(Ordering::Acquire) == DRAINING;
        let elapsed = context
            .clock
            .monotonic()
            .saturating_duration_since(context.origin.monotonic);
        let boundary = window_boundary(
            interval,
            sequence.checked_add(1).ok_or_else(|| {
                ProfilerError::InternalInvariant("window sequence overflow".to_owned())
            })?,
        )?;
        let timeout = if draining {
            Duration::ZERO
        } else {
            boundary.saturating_sub(elapsed).min(POLL)
        };
        batch.clear();
        window.record_wakeup();
        if let Err(error) =
            context
                .source
                .read_batch(timeout, context.limits.max_raw_events_queued, &mut batch)
        {
            window.record_source_loss(
                batch.lost_events,
                batch.malformed_events,
                batch.capacity_drops,
            );
            window.discard(batch.samples.len() as u64, DropReason::SourceFailure);
            context
                .shared
                .update(|stats| stats.source_errors = stats.source_errors.saturating_add(1))?;
            context.shared.fail(context.plan.id, &error);
            drop_window(
                &context.shared,
                window.statistics(),
                DropReason::SourceFailure,
            )?;
            return Err(error);
        }
        if batch.samples.len() > context.limits.max_raw_events_queued {
            window.discard(batch.samples.len() as u64, DropReason::SourceFailure);
            drop_window(
                &context.shared,
                window.statistics(),
                DropReason::SourceFailure,
            )?;
            return Err(ProfilerError::InternalInvariant(
                "event source exceeded batch limit".to_owned(),
            ));
        }
        window.record_source_loss(
            batch.lost_events,
            batch.malformed_events,
            batch.capacity_drops,
        );
        for (index, sample) in batch.samples.iter().enumerate() {
            if context.plan.cpus.binary_search(&sample.cpu).is_err() {
                window.record_source_loss(0, 1, 0);
            } else if context.shared.expired()? {
                window.discard(1, DropReason::ShutdownDeadline);
            } else {
                match window.record(sample) {
                    Ok(true) => match context.clock.event_time_ns() {
                        Ok(Some(now)) => {
                            if let Some(latency) = now.checked_sub(sample.timestamp_ns) {
                                window.record_latency(latency);
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            window.discard(
                                (batch.samples.len() - index - 1) as u64,
                                DropReason::SourceFailure,
                            );
                            drop_window(
                                &context.shared,
                                window.statistics(),
                                DropReason::SourceFailure,
                            )?;
                            return Err(error);
                        }
                    },
                    Ok(false) => {}
                    Err(error) => {
                        window.discard(
                            (batch.samples.len() - index - 1) as u64,
                            DropReason::SourceFailure,
                        );
                        drop_window(
                            &context.shared,
                            window.statistics(),
                            DropReason::SourceFailure,
                        )?;
                        return Err(error);
                    }
                }
            }
        }
        let draining = context.shared.phase.load(Ordering::Acquire) == DRAINING;
        let elapsed = context
            .clock
            .monotonic()
            .saturating_duration_since(context.origin.monotonic);
        if draining {
            if !batch.has_more {
                break;
            }
        } else if elapsed >= boundary {
            let end = context.origin.wall_at(boundary)?;
            send_window(&context, sequence, window, end)?;
            sequence = u64::try_from(elapsed.as_nanos() / interval.as_nanos()).map_err(|_| {
                ProfilerError::InternalInvariant("window sequence overflow".to_owned())
            })?;
            start = context
                .origin
                .wall_at(window_boundary(interval, sequence)?)?;
            window = new_window(&context, start)?;
        }
    }
    let end = context
        .origin
        .wall_at(
            context
                .clock
                .monotonic()
                .saturating_duration_since(context.origin.monotonic),
        )?
        .max(start);
    send_window(&context, sequence, window, end)
}

fn window_boundary(interval: Duration, sequence: u64) -> Result<Duration> {
    let nanos = interval
        .as_nanos()
        .checked_mul(u128::from(sequence))
        .ok_or_else(|| ProfilerError::InternalInvariant("window duration overflow".to_owned()))?;
    let seconds = u64::try_from(nanos / 1_000_000_000)
        .map_err(|_| ProfilerError::InternalInvariant("window duration overflow".to_owned()))?;
    Ok(Duration::new(seconds, (nanos % 1_000_000_000) as u32))
}

fn send_window(
    context: &WorkerContext,
    sequence: u64,
    window: AggregationWindow,
    end: SystemTime,
) -> Result<()> {
    let Some(permit) = context.shared.pool.acquire() else {
        return drop_window(
            &context.shared,
            window.statistics(),
            DropReason::ShardQueueFull,
        );
    };
    let pending = ShardWindow {
        sequence,
        shard: context.plan.id,
        end,
        window,
        _permit: permit,
    };
    match context.sender.try_send(pending) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(pending)) => drop_window(
            &context.shared,
            pending.window.statistics(),
            DropReason::ShardQueueFull,
        ),
        Err(TrySendError::Disconnected(pending)) => {
            drop_window(
                &context.shared,
                pending.window.statistics(),
                DropReason::ConsumerDisconnected,
            )?;
            Err(ProfilerError::SnapshotConsumerDisconnected)
        }
    }
}

fn drop_window(shared: &Shared, mut stats: ProfilerStatistics, reason: DropReason) -> Result<()> {
    stats.samples_in_snapshots = 0;
    stats.snapshots_dropped = stats.snapshots_dropped.saturating_add(1);
    stats.record_drop(reason, stats.samples_accepted);
    shared.update(|cumulative| cumulative.merge(&stats))
}

struct Generation {
    first_arrival: Instant,
    windows: Vec<ShardWindow>,
}

fn finalizer_loop(
    receiver: Receiver<ShardWindow>,
    sender: SyncSender<ProfileSnapshot>,
    shard_count: usize,
    config: &ValidatedConfig,
    shared: &Shared,
) -> Result<()> {
    let mut pending: BTreeMap<u64, Generation> = BTreeMap::new();
    let mut watermark = 0u64;
    loop {
        let disconnected = match receiver.recv_timeout(POLL) {
            Ok(window) => {
                if window.sequence < watermark {
                    drop_window(
                        shared,
                        window.window.statistics(),
                        DropReason::ShardQueueFull,
                    )?;
                } else {
                    let generation = pending
                        .entry(window.sequence)
                        .or_insert_with(|| Generation {
                            first_arrival: Instant::now(),
                            windows: Vec::new(),
                        });
                    if window.shard >= shard_count
                        || generation
                            .windows
                            .iter()
                            .any(|old| old.shard == window.shard)
                    {
                        return Err(ProfilerError::InternalInvariant(
                            "duplicate or invalid shard window".to_owned(),
                        ));
                    }
                    generation.windows.push(window);
                }
                false
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => true,
        };
        while let Some((&sequence, generation)) = pending.first_key_value() {
            let ready = generation.windows.len() == shard_count;
            let expired = generation.first_arrival.elapsed() >= config.get().reporting_interval;
            if !ready && !expired && !disconnected {
                break;
            }
            let generation = pending.remove(&sequence).ok_or_else(|| {
                ProfilerError::InternalInvariant("pending generation disappeared".to_owned())
            })?;
            watermark = sequence.checked_add(1).ok_or_else(|| {
                ProfilerError::InternalInvariant("generation sequence overflow".to_owned())
            })?;
            if let Err(error) =
                finalize_generation(generation.windows, &sender, &config.get().limits, shared)
            {
                shared.fail(usize::MAX, &error);
                for (_, generation) in pending {
                    for window in generation.windows {
                        drop_window(
                            shared,
                            window.window.statistics(),
                            DropReason::SourceFailure,
                        )?;
                    }
                }
                for window in receiver.try_iter() {
                    drop_window(
                        shared,
                        window.window.statistics(),
                        DropReason::SourceFailure,
                    )?;
                }
                return Err(error);
            }
        }
        if disconnected {
            break;
        }
    }
    Ok(())
}

fn finalize_generation(
    mut windows: Vec<ShardWindow>,
    sender: &SyncSender<ProfileSnapshot>,
    limits: &Limits,
    shared: &Shared,
) -> Result<()> {
    let began = Instant::now();
    windows.sort_unstable_by_key(|window| window.shard);
    let mut snapshots = Vec::new();
    snapshots
        .try_reserve_exact(windows.len())
        .map_err(|_| ProfilerError::Allocation("shard snapshots"))?;
    let mut permits = Vec::new();
    let mut counters = ProfilerStatistics::default();
    for window in &windows {
        counters.merge(&window.window.statistics());
    }
    for window in windows {
        permits.push(window._permit);
        match window.window.finish(window.end) {
            Ok(snapshot) => snapshots.push(snapshot),
            Err(error) => {
                drop_window(shared, counters, DropReason::SourceFailure)?;
                return Err(error);
            }
        }
    }
    let mut snapshot = match merge_snapshots(snapshots, limits) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            drop_window(shared, counters, DropReason::SourceFailure)?;
            return Err(error);
        }
    };
    snapshot.statistics.reporting_duration_ns =
        u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let mut stats = snapshot.statistics.clone();
    match sender.try_send(snapshot) {
        Ok(()) => {}
        Err(TrySendError::Full(_snapshot)) => {
            stats.snapshots_dropped = stats.snapshots_dropped.saturating_add(1);
            stats.record_drop(DropReason::SnapshotQueueFull, stats.samples_in_snapshots);
            stats.samples_in_snapshots = 0;
        }
        Err(TrySendError::Disconnected(_snapshot)) => {
            stats.snapshots_dropped = stats.snapshots_dropped.saturating_add(1);
            stats.record_drop(DropReason::ConsumerDisconnected, stats.samples_in_snapshots);
            stats.samples_in_snapshots = 0;
        }
    }
    drop(permits);
    shared.update(|cumulative| cumulative.merge(&stats))
}

#[derive(Debug)]
struct DisabledSymbols;
impl SymbolResolver for DisabledSymbols {
    fn resolve(&mut self, _path: &Path, _address: u64) -> Result<Option<crate::ResolvedSymbol>> {
        Ok(None)
    }
}

fn per_shard_limits(limits: &Limits, count: usize) -> Result<Vec<Limits>> {
    if count == 0 || count > limits.max_shards || limits.max_pending_shard_windows < count {
        return Err(ProfilerError::invalid(
            "limits",
            "pending window slots must cover every active shard",
        ));
    }
    let mut result = Vec::new();
    for shard in 0..count {
        let mut next = limits.clone();
        for (target, value) in [
            (&mut next.max_processes, limits.max_processes),
            (&mut next.max_threads, limits.max_threads),
            (&mut next.max_mappings, limits.max_mappings),
            (
                &mut next.max_unique_user_stacks,
                limits.max_unique_user_stacks,
            ),
            (
                &mut next.max_unique_kernel_stacks,
                limits.max_unique_kernel_stacks,
            ),
            (&mut next.max_functions, limits.max_functions),
            (&mut next.max_locations, limits.max_locations),
            (&mut next.max_symbols, limits.max_symbols),
            (&mut next.max_aggregation_keys, limits.max_aggregation_keys),
            (
                &mut next.max_samples_per_snapshot,
                limits.max_samples_per_snapshot,
            ),
            (&mut next.max_snapshot_bytes, limits.max_snapshot_bytes),
            (&mut next.max_error_details, limits.max_error_details),
        ] {
            *target = value / count + usize::from(shard < value % count);
        }
        next.validate()?;
        result.push(next);
    }
    Ok(result)
}

fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    mutex
        .lock()
        .map_err(|_| ProfilerError::InternalInvariant("profiler lock poisoned".to_owned()))
}

fn lock_until<T>(mutex: &Mutex<T>, deadline: Instant) -> Result<MutexGuard<'_, T>> {
    loop {
        match mutex.try_lock() {
            Ok(value) => return Ok(value),
            Err(TryLockError::Poisoned(_)) => {
                return Err(ProfilerError::InternalInvariant(
                    "profiler lock poisoned".to_owned(),
                ));
            }
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(ProfilerError::ShutdownTimeout);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

fn wait_for(handles: &[JoinHandle<Result<()>>], deadline: Option<Instant>) -> Result<()> {
    while handles.iter().any(|handle| !handle.is_finished()) {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(ProfilerError::ShutdownTimeout);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn native_platform() -> Arc<dyn Platform> {
    crate::linux::LinuxPlatform::shared()
}

#[cfg(target_os = "linux")]
fn native_event_time() -> Result<Option<u64>> {
    crate::linux::boot_time_ns().map(Some)
}

#[cfg(not(target_os = "linux"))]
fn native_platform() -> Arc<dyn Platform> {
    Arc::new(UnsupportedPlatform)
}

#[cfg(not(target_os = "linux"))]
fn native_event_time() -> Result<Option<u64>> {
    Ok(None)
}

#[cfg(not(target_os = "linux"))]
struct UnsupportedPlatform;
#[cfg(not(target_os = "linux"))]
impl Platform for UnsupportedPlatform {
    fn discover_topology(&self, _config: &ValidatedConfig) -> Result<crate::SystemTopology> {
        Err(ProfilerError::UnsupportedOperatingSystem(
            std::env::consts::OS.to_owned(),
        ))
    }
    fn prepare(
        &self,
        _config: &ValidatedConfig,
        _shards: &[ShardPlan],
    ) -> Result<crate::PreparedPlatform> {
        Err(ProfilerError::UnsupportedOperatingSystem(
            std::env::consts::OS.to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CpuInfo, EventFlags, MAX_ABI_STACK_DEPTH, MappingTable, PlatformStart, PreparedPlatform,
        ProcessIdentity, ProcessMetadata, RawSample, ShardingStrategy, SyntheticEventSource,
        SyntheticPlatform, SystemTopology, ThreadMetadata,
    };
    use std::sync::atomic::AtomicU64;

    static TEST_LEASE: Mutex<()> = Mutex::new(());
    fn isolated() -> MutexGuard<'static, ()> {
        TEST_LEASE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    struct Processes;
    impl ProcessProvider for Processes {
        fn process(&self, pid: u32) -> Result<ProcessMetadata> {
            Ok(ProcessMetadata {
                identity: ProcessIdentity {
                    pid,
                    start_time_ticks: 1,
                },
                name: "synthetic".to_owned(),
                executable: None,
                mappings: MappingTable::default(),
                issues: crate::MetadataIssues::default(),
            })
        }
        fn thread(&self, process: ProcessIdentity, tid: u32) -> Result<ThreadMetadata> {
            Ok(ThreadMetadata {
                process,
                tid,
                start_time_ticks: 1,
                name: None,
                issues: crate::MetadataIssues::default(),
            })
        }
    }
    fn event() -> RawSample {
        let mut user_frames = [0; MAX_ABI_STACK_DEPTH];
        user_frames[0] = 0x1000;
        RawSample {
            pid: 1,
            tid: 1,
            cpu: 0,
            timestamp_ns: 1,
            flags: EventFlags::empty(),
            user_stack_error: 0,
            kernel_stack_error: 0,
            user_depth: 1,
            kernel_depth: 0,
            user_frames,
            kernel_frames: [0; MAX_ABI_STACK_DEPTH],
        }
    }
    fn topology(cpus: u32) -> SystemTopology {
        SystemTopology::new(
            (0..cpus)
                .map(|id| CpuInfo {
                    id,
                    online: true,
                    numa_node: None,
                })
                .collect(),
        )
        .expect("fixture topology is valid")
    }
    fn config() -> ProfilerConfig {
        ProfilerConfig {
            sharding: ShardingStrategy::Single,
            affinity: AffinityMode::Disabled,
            reporting_interval: Duration::from_millis(10),
            ..ProfilerConfig::default()
        }
    }
    fn builder(batches: Vec<EventBatch>) -> ProfilerBuilder {
        ProfilerBuilder::new(config())
            .with_platform(Arc::new(SyntheticPlatform::new(
                topology(1),
                vec![SyntheticEventSource::new(batches)],
            )))
            .with_process_provider(Arc::new(Processes))
    }
    fn shutdown(profiler: &Profiler) -> ShutdownReport {
        profiler
            .shutdown(Instant::now() + Duration::from_secs(2))
            .expect("workers terminate")
    }

    /// Scenario: A synthetic source traverses the same worker/finalizer handoff
    /// used by the Linux backend.
    /// Guarantees: The consumer owns a valid nonempty graph with exact counts.
    #[test]
    fn synthetic_pipeline_produces_owned_snapshot() {
        let _lease = isolated();
        let (profiler, receiver) = builder(vec![EventBatch {
            samples: vec![event()],
            ..EventBatch::default()
        }])
        .start()
        .expect("fixture starts");
        let snapshot = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("window arrives");
        snapshot.validate().expect("graph is valid");
        assert_eq!(snapshot.statistics.samples_in_snapshots, 1);
        let report = shutdown(&profiler);
        assert!(report.cleanup_complete);
        assert!(report.failure.is_none());
    }

    /// Scenario: A profiler owns the process-local lease through cleanup.
    /// Guarantees: Conflicting starts fail, repeated shutdown is idempotent,
    /// and a subsequent instance can start after all threads have joined.
    #[test]
    fn singleton_and_repeated_shutdown() {
        let _lease = isolated();
        let (profiler, _receiver) = builder(Vec::new()).start().expect("first instance starts");
        assert!(matches!(
            builder(Vec::new()).start(),
            Err(ProfilerError::AlreadyRunning)
        ));
        let first = shutdown(&profiler);
        let second = shutdown(&profiler);
        assert_eq!(first.workers_joined, second.workers_joined);
        let (next, _receiver) = builder(Vec::new()).start().expect("lease was released");
        assert!(shutdown(&next).cleanup_complete);
    }

    /// Scenario: A batch contains kernel lost notifications and valid samples.
    /// Guarantees: Kernel loss is diagnostic and is not counted twice as
    /// userspace sample loss.
    #[test]
    fn kernel_loss_is_separate_from_userspace_loss() {
        let _lease = isolated();
        let (profiler, receiver) = builder(vec![EventBatch {
            samples: vec![event()],
            lost_events: 3,
            ..EventBatch::default()
        }])
        .start()
        .expect("fixture starts");
        let snapshot = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("window arrives");
        assert_eq!(snapshot.statistics.kernel_lost_events, 3);
        assert_eq!(snapshot.statistics.userspace_dropped(), 0);
        let _report = shutdown(&profiler);
    }

    /// Scenario: Two NUMA shards collect the same process on disjoint CPUs.
    /// Guarantees: Both workers are used, metadata merges, and CPU sample
    /// dimensions survive the full ownership handoff.
    #[test]
    fn per_numa_workers_produce_one_valid_owned_generation() {
        let _lease = isolated();
        let topology = SystemTopology::new(vec![
            CpuInfo {
                id: 0,
                online: true,
                numa_node: Some(crate::NumaNodeId(0)),
            },
            CpuInfo {
                id: 1,
                online: true,
                numa_node: Some(crate::NumaNodeId(1)),
            },
        ])
        .expect("two-node topology");
        let mut second = event();
        second.cpu = 1;
        let source = Arc::new(SyntheticPlatform::new(
            topology,
            vec![
                SyntheticEventSource::new(vec![EventBatch {
                    samples: vec![event()],
                    ..EventBatch::default()
                }]),
                SyntheticEventSource::new(vec![EventBatch {
                    samples: vec![second],
                    ..EventBatch::default()
                }]),
            ],
        ));
        let config = ProfilerConfig {
            sharding: ShardingStrategy::PerNuma,
            reporting_interval: Duration::from_millis(50),
            ..config()
        };
        let (profiler, receiver) = ProfilerBuilder::new(config)
            .with_platform(source)
            .with_process_provider(Arc::new(Processes))
            .start()
            .expect("NUMA workers start");
        let snapshot = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("merged window");
        snapshot.validate().expect("graph is valid");
        assert_eq!(snapshot.samples.len(), 2);
        assert_eq!(snapshot.processes.len(), 1);
        assert_eq!(snapshot.stacks.len(), 1);
        let report = shutdown(&profiler);
        assert_eq!(report.workers_joined, 2);
        assert_eq!(report.statistics.raw_events_received, 2);
    }

    /// Scenario: A source returns a record whose CPU is outside its shard.
    /// Guarantees: The record is counted as malformed instead of misattributed.
    #[test]
    fn event_cpu_must_belong_to_its_source_shard() {
        let _lease = isolated();
        let mut wrong = event();
        wrong.cpu = 99;
        let (profiler, receiver) = builder(vec![EventBatch {
            samples: vec![wrong, event()],
            ..EventBatch::default()
        }])
        .start()
        .expect("fixture starts");
        let snapshot = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("window arrives");
        assert_eq!(snapshot.statistics.raw_events_received, 2);
        assert_eq!(
            snapshot.statistics.dropped_by_reason[DropReason::MalformedEvent as usize],
            1
        );
        assert_eq!(snapshot.statistics.samples_in_snapshots, 1);
        let _report = shutdown(&profiler);
    }

    struct TickClock {
        origin: Instant,
        nanos: AtomicU64,
    }
    impl Clock for TickClock {
        fn monotonic(&self) -> Instant {
            self.origin + Duration::from_nanos(self.nanos.load(Ordering::Acquire))
        }
        fn wall_time(&self) -> SystemTime {
            SystemTime::UNIX_EPOCH
        }
    }
    struct RepeatingSource {
        clock: Arc<TickClock>,
    }
    impl EventSource for RepeatingSource {
        fn read_batch(
            &mut self,
            _timeout: Duration,
            _max: usize,
            batch: &mut EventBatch,
        ) -> Result<()> {
            batch.clear();
            batch.samples.push(event());
            let _previous = self.clock.nanos.fetch_add(10_000_000, Ordering::AcqRel);
            Ok(())
        }
    }
    struct TestPlatform {
        sources: Mutex<Option<Vec<Box<dyn EventSource>>>>,
        cpus: u32,
        cleaned: Arc<AtomicBool>,
        started: Arc<AtomicBool>,
        fail_enable: bool,
    }
    struct TestGuard {
        cleaned: Arc<AtomicBool>,
        started: Arc<AtomicBool>,
        fail_enable: bool,
        cpus: u32,
    }
    impl Drop for TestGuard {
        fn drop(&mut self) {
            self.cleaned.store(true, Ordering::Release);
        }
    }
    impl PlatformGuard for TestGuard {
        fn start_sampling(&mut self) -> Result<PlatformStart> {
            if self.fail_enable {
                return Err(ProfilerError::ProgramAttachment {
                    cpu: 0,
                    reason: "injected attachment failure".to_owned(),
                });
            }
            self.started.store(true, Ordering::Release);
            Ok(PlatformStart {
                attached_cpus: self.cpus as usize,
                unavailable_cpus: 0,
            })
        }
        fn stop_sampling(&mut self) -> Result<()> {
            Ok(())
        }
        fn cleanup(&mut self) -> Result<()> {
            self.cleaned.store(true, Ordering::Release);
            Ok(())
        }
    }
    impl Platform for TestPlatform {
        fn discover_topology(&self, _config: &ValidatedConfig) -> Result<SystemTopology> {
            Ok(topology(self.cpus))
        }
        fn prepare(
            &self,
            _config: &ValidatedConfig,
            _shards: &[ShardPlan],
        ) -> Result<PreparedPlatform> {
            Ok(PreparedPlatform {
                sources: lock(&self.sources)?
                    .take()
                    .ok_or_else(|| ProfilerError::PartialStartup("prepared twice".to_owned()))?,
                guard: Box::new(TestGuard {
                    cleaned: Arc::clone(&self.cleaned),
                    started: Arc::clone(&self.started),
                    fail_enable: self.fail_enable,
                    cpus: self.cpus,
                }),
            })
        }
    }
    fn platform(sources: Vec<Box<dyn EventSource>>, cpus: u32) -> Arc<TestPlatform> {
        Arc::new(TestPlatform {
            sources: Mutex::new(Some(sources)),
            cpus,
            fail_enable: false,
            cleaned: Arc::new(AtomicBool::new(false)),
            started: Arc::new(AtomicBool::new(false)),
        })
    }
    fn wait_until(mut predicate: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(2);
        while !predicate() {
            assert!(
                Instant::now() < until,
                "bounded condition should become true"
            );
            std::thread::yield_now();
        }
    }

    /// Scenario: An injected clock creates windows faster than a one-slot
    /// consumer queue can drain.
    /// Guarantees: Queue pressure loses counted samples, not unbounded memory.
    #[test]
    fn slow_and_disconnected_consumers_are_bounded() {
        let _lease = isolated();
        let clock = Arc::new(TickClock {
            origin: Instant::now(),
            nanos: AtomicU64::new(0),
        });
        let platform = platform(
            vec![Box::new(RepeatingSource {
                clock: Arc::clone(&clock),
            })],
            1,
        );
        let mut config = config();
        config.limits.max_pending_snapshots = 1;
        let (profiler, receiver) = ProfilerBuilder::new(config)
            .with_platform(platform)
            .with_clock(clock)
            .with_process_provider(Arc::new(Processes))
            .start()
            .expect("fixture starts");
        wait_until(|| {
            profiler
                .statistics()
                .expect("stats are available")
                .dropped_by_reason[DropReason::SnapshotQueueFull as usize]
                > 0
        });
        drop(receiver);
        wait_until(|| {
            profiler
                .statistics()
                .expect("stats are available")
                .dropped_by_reason[DropReason::ConsumerDisconnected as usize]
                > 0
        });
        let report = shutdown(&profiler);
        assert!(report.cleanup_complete);
        assert!(report.statistics.dropped_by_reason[DropReason::SnapshotQueueFull as usize] > 0);
        assert_eq!(
            report.statistics.raw_events_received,
            report.statistics.samples_in_snapshots + report.statistics.userspace_dropped()
        );
    }

    /// Scenario: A prepared platform returns the wrong number of sources.
    /// Guarantees: Startup drops all resources and releases its singleton lease.
    #[test]
    fn invalid_preparation_rolls_back() {
        let _lease = isolated();
        let platform = platform(Vec::new(), 1);
        let cleaned = Arc::clone(&platform.cleaned);
        let result = ProfilerBuilder::new(config())
            .with_platform(platform)
            .start();
        assert!(matches!(result, Err(ProfilerError::PartialStartup(_))));
        assert!(cleaned.load(Ordering::Acquire));
        let (profiler, _receiver) = builder(Vec::new()).start().expect("lease is reusable");
        let _report = shutdown(&profiler);
    }

    struct FailSecondAffinity;
    impl WorkerAffinity for FailSecondAffinity {
        fn pin(&self, cpu: u32) -> Result<()> {
            if cpu == 1 {
                Err(ProfilerError::WorkerStartup {
                    shard: 1,
                    reason: "injected affinity failure".to_owned(),
                })
            } else {
                Ok(())
            }
        }
    }

    /// Scenario: One worker initializes and another fails required affinity.
    /// Guarantees: No sampling is enabled and every partial worker is joined.
    #[test]
    fn partial_worker_startup_rolls_back_before_enable() {
        let _lease = isolated();
        let platform = platform(
            vec![
                Box::new(SyntheticEventSource::new(Vec::new())),
                Box::new(SyntheticEventSource::new(Vec::new())),
            ],
            2,
        );
        let cleaned = Arc::clone(&platform.cleaned);
        let started = Arc::clone(&platform.started);
        let config = ProfilerConfig {
            sharding: ShardingStrategy::Fixed {
                workers: std::num::NonZeroUsize::new(2).expect("nonzero"),
            },
            affinity: AffinityMode::Required,
            ..config()
        };
        assert!(
            ProfilerBuilder::new(config)
                .with_platform(platform)
                .with_affinity(Arc::new(FailSecondAffinity))
                .start()
                .is_err()
        );
        assert!(cleaned.load(Ordering::Acquire));
        assert!(!started.load(Ordering::Acquire));
        assert!(!PROFILER_LEASE.load(Ordering::Acquire));
    }

    struct CountReads(Arc<AtomicUsize>);
    impl EventSource for CountReads {
        fn read_batch(
            &mut self,
            _timeout: Duration,
            _max: usize,
            batch: &mut EventBatch,
        ) -> Result<()> {
            let _old = self.0.fetch_add(1, Ordering::AcqRel);
            batch.clear();
            Ok(())
        }
    }

    /// Scenario: Enabling perf events fails after workers have initialized.
    /// Guarantees: Rollback never invokes drain callbacks for an uncommitted
    /// startup, joins workers, and releases prepared resources.
    #[test]
    fn enable_failure_rolls_back_without_draining_unstarted_sources() {
        let _lease = isolated();
        let reads = Arc::new(AtomicUsize::new(0));
        let cleaned = Arc::new(AtomicBool::new(false));
        let platform = Arc::new(TestPlatform {
            sources: Mutex::new(Some(vec![Box::new(CountReads(Arc::clone(&reads)))])),
            cpus: 1,
            cleaned: Arc::clone(&cleaned),
            started: Arc::new(AtomicBool::new(false)),
            fail_enable: true,
        });
        assert!(matches!(
            ProfilerBuilder::new(config())
                .with_platform(platform)
                .start(),
            Err(ProfilerError::ProgramAttachment { .. }),
        ));
        assert_eq!(reads.load(Ordering::Acquire), 0);
        assert!(cleaned.load(Ordering::Acquire));
        assert!(!PROFILER_LEASE.load(Ordering::Acquire));
    }

    struct PausedSource {
        entered: SyncSender<()>,
        release: Receiver<()>,
        dropped: Arc<AtomicBool>,
    }
    impl Drop for PausedSource {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }
    impl EventSource for PausedSource {
        fn read_batch(
            &mut self,
            _timeout: Duration,
            _max: usize,
            batch: &mut EventBatch,
        ) -> Result<()> {
            self.entered.send(()).map_err(|_| {
                ProfilerError::InternalInvariant("fixture notification closed".to_owned())
            })?;
            self.release
                .recv_timeout(Duration::from_secs(2))
                .map_err(|_| {
                    ProfilerError::InternalInvariant("fixture release timed out".to_owned())
                })?;
            batch.clear();
            batch.samples.push(event());
            Ok(())
        }
    }

    /// Scenario: Shutdown times out while a source has one in-flight sample.
    /// Guarantees: Retry joins the source, counts that sample once as deadline
    /// loss, and preserves the joined-worker count.
    #[test]
    fn deadline_retry_retains_worker_ownership_and_exact_loss() {
        let _lease = isolated();
        let (entered_tx, entered_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        let dropped = Arc::new(AtomicBool::new(false));
        let platform = platform(
            vec![Box::new(PausedSource {
                entered: entered_tx,
                release: release_rx,
                dropped: Arc::clone(&dropped),
            })],
            1,
        );
        let (profiler, _receiver) = ProfilerBuilder::new(config())
            .with_platform(platform)
            .with_process_provider(Arc::new(Processes))
            .start()
            .expect("fixture starts");
        entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("read is in flight");
        assert!(matches!(
            profiler.shutdown(Instant::now()),
            Err(ProfilerError::ShutdownTimeout)
        ));
        assert!(!dropped.load(Ordering::Acquire));
        assert!(matches!(
            builder(Vec::new()).start(),
            Err(ProfilerError::AlreadyRunning)
        ));
        release_tx.send(()).expect("release worker");
        let report = shutdown(&profiler);
        assert_eq!(report.workers_joined, 1);
        assert_eq!(report.statistics.shutdown_losses, 1);
        assert_eq!(report.statistics.shutdown_timeouts, 1);
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(shutdown(&profiler).workers_joined, 1);
    }

    struct FailedEventClock;
    impl Clock for FailedEventClock {
        fn monotonic(&self) -> Instant {
            Instant::now()
        }
        fn wall_time(&self) -> SystemTime {
            SystemTime::now()
        }
        fn event_time_ns(&self) -> Result<Option<u64>> {
            Err(ProfilerError::InternalInvariant(
                "injected event-clock failure".to_owned(),
            ))
        }
    }

    /// Scenario: A clock fails after the first sample in a decoded batch.
    /// Guarantees: Both accepted state and unprocessed batch records are
    /// accounted as loss before the worker exits.
    #[test]
    fn mid_batch_failure_accounts_for_every_decoded_sample() {
        let _lease = isolated();
        let (profiler, _receiver) = builder(vec![EventBatch {
            samples: vec![event(), event(), event()],
            ..EventBatch::default()
        }])
        .with_clock(Arc::new(FailedEventClock))
        .start()
        .expect("fixture starts");
        wait_until(|| profiler.failure().is_some());
        let report = shutdown(&profiler);
        assert_eq!(report.statistics.raw_events_received, 3);
        assert_eq!(report.statistics.userspace_dropped(), 3);
        assert_eq!(report.statistics.samples_in_snapshots, 0);
        assert!(report.cleanup_complete);
    }

    struct FailingSource;
    impl EventSource for FailingSource {
        fn read_batch(
            &mut self,
            _timeout: Duration,
            _max: usize,
            _batch: &mut EventBatch,
        ) -> Result<()> {
            Err(ProfilerError::ProgramLoad(
                "injected source failure".to_owned(),
            ))
        }
    }

    /// Scenario: A running source returns a terminal error.
    /// Guarantees: The receiver exposes a typed worker failure, ingress stops,
    /// and cleanup succeeds instead of silently producing empty windows.
    #[test]
    fn source_failure_is_visible_to_consumer_and_shutdown() {
        let _lease = isolated();
        let platform = platform(vec![Box::new(FailingSource)], 1);
        let (profiler, receiver) = ProfilerBuilder::new(config())
            .with_platform(platform)
            .with_process_provider(Arc::new(Processes))
            .start()
            .expect("fixture starts");
        wait_until(|| profiler.failure().is_some());
        assert!(matches!(
            receiver.try_recv(),
            Err(ProfilerError::WorkerFailed { .. })
        ));
        let report = shutdown(&profiler);
        assert!(report.cleanup_complete);
        assert!(report.failure.is_some());
        assert_eq!(report.statistics.source_errors, 1);
    }

    /// Scenario: A non-even capacity is split across two shards.
    /// Guarantees: Quotas sum to the global limit without ceiling duplication.
    #[test]
    fn quotas_preserve_global_capacity() {
        let limits = Limits {
            max_processes: 5,
            ..Limits::default()
        };
        let quotas = per_shard_limits(&limits, 2).expect("quotas are valid");
        assert_eq!((quotas[0].max_processes, quotas[1].max_processes), (3, 2));
    }

    /// Scenario: Reporting runs beyond the u32 window-count range.
    /// Guarantees: Long-running window boundaries do not wrap or freeze.
    #[test]
    fn reporting_sequence_uses_checked_u64_arithmetic() {
        let boundary = window_boundary(Duration::from_millis(10), u64::from(u32::MAX) + 2)
            .expect("boundary is representable");
        assert_eq!(
            boundary,
            Duration::from_millis((u64::from(u32::MAX) + 2) * 10)
        );
    }
}
