// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Reclaim-hook foundation (design "Reclaim Hooks", Phase 2e).
//!
//! Stateful retention sites (batch processors, retry buffers, durable buffers,
//! topic queues, stream state, delayed scheduler payloads) can release retained
//! memory on demand when a runtime is under pressure. This module provides the
//! engine-side primitives for that, intentionally scoped to the *mechanism*:
//!
//! - [`LocalMemoryReclaim`]: the `!Send`, **synchronous** reclaim trait a
//!   stateful component implements. Reclaim is non-blocking and releases memory
//!   only through RAII drop of owners the component already holds, so it cannot
//!   await, yield, or acquire budget.
//! - [`ReclaimContext`]: a **budget-free** capability token. It exposes only
//!   inspection; it has no method to reserve, charge, or otherwise acquire
//!   budget, so a reclaim implementation cannot accidentally grow the budget
//!   while it is supposed to be shrinking it. Release happens through RAII drop
//!   of the component's own [`LocalMemoryTicket`](super::LocalMemoryTicket) /
//!   [`EscrowTicket`](super::EscrowTicket) owners.
//! - [`ReclaimCoordinator`]: drives a set of reclaimers in deterministic
//!   [`ReclaimPriority`] order, stops once the target byte count is met or all
//!   reclaimers report no progress, and refuses re-entry on the same runtime
//!   thread.
//! - [`ReclaimRegistry`]: a runtime-local registration lifecycle over the
//!   coordinator (register/unregister with RAII teardown), installed per pinned
//!   pipeline thread and reachable via [`current_reclaim_registry`].
//!
//! This is the Phase 2e *foundation/mechanism*. A registry is installed per
//! runtime, but **no concrete site registers a reclaimer and no driver calls
//! reclaim yet**. Wiring concrete reclaimers (and enabling reclaim-driven
//! enforcement) is gated behind `enforcement.reclaim_hooks`, which the config
//! layer rejects until a reclaimer and a driver are wired end to end. Keeping
//! the mechanism separate lets it be reviewed and tested in isolation before any
//! site depends on it.
//!
//! First-site selection note: the retry processor was evaluated as the first
//! concrete reclaimer and rejected. It owns the per-payload `LocalMemoryTicket`s
//! but **not** the retained payload, which `requeue_later` moves into the
//! engine's `node_local_scheduler` delayed-resumes heap. With no scheduler
//! cancel-resume API and no scheduler access from a registry-driven reclaimer, a
//! retry reclaimer could only drop tickets — lowering `charged_bytes` without
//! freeing the retained `OtapPdata` or shedding data, which would violate the
//! "release only by dropping owners already held" contract. A valid first site
//! must own both the retained data and its ticket so reclaim drops them
//! together (see `retry_processor::RetryProcessor::retry_budget_tickets` and the
//! Reclaim Hooks section of `docs/memory-limiter-phase2.md`).

use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::rc::{Rc, Weak};

/// Deterministic ordering class for reclaim attempts.
///
/// The coordinator asks reclaimers in declaration order (queues first, then
/// processors, buffers, and finally stream state), using the registration
/// index as a stable tie breaker. Releasing queued/buffered bytes before
/// stream state keeps the most reconstructible memory the first to go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ReclaimPriority {
    /// Local or shared queue depth.
    Queue,
    /// Batch/aggregating processor buffers.
    Processor,
    /// Standalone retained buffers (e.g. retry, durable).
    Buffer,
    /// Long-lived stream/session state.
    Stream,
}

/// Outcome of a single [`LocalMemoryReclaim::reclaim`] call.
///
/// `attempted_bytes` lets the coordinator distinguish "tried and could not
/// release" from "released part of the request", so it can avoid immediately
/// re-polling a reclaimer that gave back far less than asked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[must_use]
pub struct ReclaimResult {
    /// Bytes the reclaimer attempted to release this call.
    pub attempted_bytes: u64,
    /// Bytes actually released this call.
    pub released_bytes: u64,
    /// Whether the reclaimer believes it has more it could release if asked
    /// again.
    pub more_available: bool,
}

impl ReclaimResult {
    /// A no-op result: nothing attempted, nothing released, nothing left.
    pub const fn no_progress() -> Self {
        Self {
            attempted_bytes: 0,
            released_bytes: 0,
            more_available: false,
        }
    }

    /// Builds a result for a reclaimer that released `released_bytes` after
    /// attempting `attempted_bytes`.
    pub const fn released(attempted_bytes: u64, released_bytes: u64, more_available: bool) -> Self {
        Self {
            attempted_bytes,
            released_bytes,
            more_available,
        }
    }
}

/// Budget-free capability token handed to a reclaimer.
///
/// By construction this exposes only inspection of the remaining target. It has
/// no method to reserve, charge, borrow, or otherwise acquire budget. A reclaim
/// implementation releases memory by dropping (or aborting) the owners it
/// already holds, never by acquiring new budget. The lifetime parameter mirrors
/// the design's borrow of engine-internal release primitives and reserves room
/// for them without widening the surface today.
pub struct ReclaimContext<'a> {
    remaining_target_bytes: u64,
    _budget_free: PhantomData<&'a ()>,
}

impl ReclaimContext<'_> {
    fn new(remaining_target_bytes: u64) -> Self {
        Self {
            remaining_target_bytes,
            _budget_free: PhantomData,
        }
    }

    /// Bytes still needed to satisfy the current reclaim pass. A reclaimer may
    /// stop early once it has released this much.
    #[must_use]
    pub fn remaining_target_bytes(&self) -> u64 {
        self.remaining_target_bytes
    }
}

/// A stateful, runtime-local component that can release retained memory.
///
/// Implementations are `!Send`: they run on the pinned pipeline runtime thread
/// alongside the local budget account. `reclaim` is **synchronous and
/// non-blocking** by design — it releases memory by dropping, draining, or
/// aborting the owners it already holds (RAII), which never blocks or awaits.
/// Keeping it synchronous makes the contract enforceable: a reclaimer cannot
/// yield to other tasks while the registry holds it borrowed, so there is no
/// borrow-across-await hazard, and reclaim stays bounded, runtime-local, and
/// budget-free. It must also tolerate being called again later.
pub trait LocalMemoryReclaim {
    /// The ordering class for this reclaimer.
    fn reclaim_priority(&self) -> ReclaimPriority;

    /// Attempts to release up to `target_bytes`. Returns how much was attempted
    /// and released and whether more is available.
    fn reclaim(&mut self, target_bytes: u64, context: ReclaimContext<'_>) -> ReclaimResult;
}

/// Result of a coordinator-driven reclaim pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[must_use]
pub struct ReclaimOutcome {
    /// Total bytes released across all reclaimers this pass.
    pub released_bytes: u64,
    /// Whether the requested target was fully met.
    pub target_met: bool,
    /// Whether the pass was refused because a reclaim was already running on
    /// this runtime thread (re-entry guard tripped). When `true` the other
    /// fields describe no work performed by this call.
    pub reentered: bool,
}

/// Drives reclaimers deterministically and prevents re-entrant reclaim on a
/// single runtime thread.
///
/// The coordinator does not own the reclaimers; callers pass the borrowed set
/// for each pass. This keeps the engine in charge of which retention sites
/// exist while the coordinator owns only the ordering, stop condition, and
/// re-entrancy guarantees.
#[derive(Debug, Default)]
pub struct ReclaimCoordinator {
    in_progress: Cell<bool>,
}

/// RAII guard that clears the re-entrancy flag when a reclaim pass ends, even
/// if a reclaimer panics or returns early.
struct ReentryGuard<'a> {
    flag: &'a Cell<bool>,
}

impl Drop for ReentryGuard<'_> {
    fn drop(&mut self) {
        self.flag.set(false);
    }
}

impl ReclaimCoordinator {
    /// Creates an idle coordinator.
    #[must_use]
    pub fn new() -> Self {
        Self {
            in_progress: Cell::new(false),
        }
    }

    /// Returns whether a reclaim pass is currently running on this thread.
    #[must_use]
    pub fn is_reclaiming(&self) -> bool {
        self.in_progress.get()
    }

    /// Drives `reclaimers` in [`ReclaimPriority`] order (stable index tie-break)
    /// until `target_bytes` is released or a full pass releases nothing.
    ///
    /// Budget-free: every reclaimer receives a [`ReclaimContext`] that cannot
    /// acquire budget. Re-entry safe: if a reclaim pass is already running on
    /// this runtime thread (for example a reclaimer that re-enters the
    /// coordinator) the nested call returns immediately with
    /// [`ReclaimOutcome::reentered`] set and performs no work, so the same hook
    /// cannot be driven concurrently.
    pub fn reclaim(
        &self,
        reclaimers: &mut [&mut dyn LocalMemoryReclaim],
        target_bytes: u64,
    ) -> ReclaimOutcome {
        if self.in_progress.replace(true) {
            return ReclaimOutcome {
                released_bytes: 0,
                target_met: false,
                reentered: true,
            };
        }
        let _guard = ReentryGuard {
            flag: &self.in_progress,
        };

        // Deterministic order: priority class first, registration index second.
        let mut order: Vec<usize> = (0..reclaimers.len()).collect();
        order.sort_by_key(|&index| (reclaimers[index].reclaim_priority(), index));

        let mut released_total: u64 = 0;
        if target_bytes == 0 {
            return ReclaimOutcome {
                released_bytes: 0,
                target_met: true,
                reentered: false,
            };
        }

        loop {
            let mut progressed = false;
            for &index in &order {
                if released_total >= target_bytes {
                    break;
                }
                let remaining = target_bytes - released_total;
                let context = ReclaimContext::new(remaining);
                let result = reclaimers[index].reclaim(remaining, context);
                released_total = released_total.saturating_add(result.released_bytes);
                if result.released_bytes > 0 {
                    progressed = true;
                }
            }
            if released_total >= target_bytes || !progressed {
                break;
            }
        }

        ReclaimOutcome {
            released_bytes: released_total,
            target_met: released_total >= target_bytes,
            reentered: false,
        }
    }

    /// Like [`reclaim`](Self::reclaim) but drives shared, runtime-local
    /// reclaimers held as `Rc<RefCell<dyn LocalMemoryReclaim>>` (the shape the
    /// [`ReclaimRegistry`] stores). Each reclaimer is borrowed only for the
    /// duration of its own (synchronous) `reclaim` call, so registered
    /// reclaimers are never all borrowed simultaneously and a reclaimer may
    /// safely register/unregister others mid-pass (changes apply to the next
    /// pass). Because `reclaim` is synchronous it cannot yield while borrowed, so
    /// no other code path on this single runtime thread can re-borrow the same
    /// cell during the call.
    pub fn reclaim_shared(
        &self,
        reclaimers: &[Rc<RefCell<dyn LocalMemoryReclaim>>],
        target_bytes: u64,
    ) -> ReclaimOutcome {
        if self.in_progress.replace(true) {
            return ReclaimOutcome {
                released_bytes: 0,
                target_met: false,
                reentered: true,
            };
        }
        let _guard = ReentryGuard {
            flag: &self.in_progress,
        };

        // Deterministic order: priority class first, registration index second.
        // Priority is read under a short shared borrow, released before the loop.
        let mut order: Vec<usize> = (0..reclaimers.len()).collect();
        order.sort_by_key(|&index| (reclaimers[index].borrow().reclaim_priority(), index));

        let mut released_total: u64 = 0;
        if target_bytes == 0 {
            return ReclaimOutcome {
                released_bytes: 0,
                target_met: true,
                reentered: false,
            };
        }

        loop {
            let mut progressed = false;
            for &index in &order {
                if released_total >= target_bytes {
                    break;
                }
                let remaining = target_bytes - released_total;
                let context = ReclaimContext::new(remaining);
                // Borrow only this reclaimer, and only across its own call.
                let result = {
                    let mut reclaimer = reclaimers[index].borrow_mut();
                    reclaimer.reclaim(remaining, context)
                };
                released_total = released_total.saturating_add(result.released_bytes);
                if result.released_bytes > 0 {
                    progressed = true;
                }
            }
            if released_total >= target_bytes || !progressed {
                break;
            }
        }

        ReclaimOutcome {
            released_bytes: released_total,
            target_met: released_total >= target_bytes,
            reentered: false,
        }
    }
}

/// One registered reclaimer plus its stable registration id.
struct RegistryEntry {
    id: u64,
    reclaimer: Rc<RefCell<dyn LocalMemoryReclaim>>,
}

#[derive(Default)]
struct ReclaimRegistryInner {
    next_id: u64,
    entries: Vec<RegistryEntry>,
}

/// Runtime-local registry of reclaimers, paired with a [`ReclaimCoordinator`].
///
/// This is the lifecycle layer over the reclaim primitive: stateful retention
/// sites register a shared-but-runtime-local reclaimer (`Rc<RefCell<dyn
/// LocalMemoryReclaim>>`) and receive a [`ReclaimRegistration`] RAII guard that
/// unregisters on drop. A reclaim pass drives every currently-registered
/// reclaimer through the coordinator in deterministic [`ReclaimPriority`] order.
///
/// The registry is `!Send` (it holds `Rc`), matching the pinned current-thread
/// runtime model: registration, unregistration, and reclaim passes all happen on
/// the same runtime thread as the local budget account. It is cheap to clone
/// (shared `Rc` handles), so a site can hold a clone to register itself and the
/// runtime can hold a clone to drive passes.
///
/// Reclaim remains budget-free: reclaimers release memory only by dropping the
/// owners they already hold, never by acquiring budget. Wiring reclaim-driven
/// admission/enforcement stays gated behind `enforcement.reclaim_hooks`.
#[derive(Clone, Default)]
pub struct ReclaimRegistry {
    inner: Rc<RefCell<ReclaimRegistryInner>>,
    coordinator: Rc<ReclaimCoordinator>,
}

impl ReclaimRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `reclaimer` and returns an RAII guard that unregisters it when
    /// dropped. The reclaimer is shared (`Rc<RefCell<_>>`) so the registering
    /// component keeps a clone to mutate its own retained state on the hot path
    /// while the registry borrows it only during a reclaim pass.
    #[must_use = "dropping the returned registration immediately unregisters the reclaimer"]
    pub fn register(&self, reclaimer: Rc<RefCell<dyn LocalMemoryReclaim>>) -> ReclaimRegistration {
        let mut inner = self.inner.borrow_mut();
        let id = inner.next_id;
        inner.next_id = inner.next_id.wrapping_add(1);
        inner.entries.push(RegistryEntry { id, reclaimer });
        ReclaimRegistration {
            registry: Rc::downgrade(&self.inner),
            id,
        }
    }

    /// Number of currently-registered reclaimers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.borrow().entries.len()
    }

    /// Whether no reclaimers are currently registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.borrow().entries.is_empty()
    }

    /// Whether a reclaim pass is currently running on this thread.
    #[must_use]
    pub fn is_reclaiming(&self) -> bool {
        self.coordinator.is_reclaiming()
    }

    /// Drives every registered reclaimer to release up to `target_bytes`.
    ///
    /// Re-entry safe: if a pass is already running on this thread (for example a
    /// reclaimer that calls back into the registry), the nested call returns
    /// immediately with [`ReclaimOutcome::reentered`] set and performs no work —
    /// without attempting to re-borrow the already-borrowed reclaimers.
    pub fn reclaim(&self, target_bytes: u64) -> ReclaimOutcome {
        // Snapshot the registered reclaimer handles so the registry inner is not
        // borrowed across the pass: a reclaimer may register or unregister
        // (mutating the registry) during the pass; such changes apply to the
        // next pass, not this one. The coordinator borrows each reclaimer only
        // for its own call and refuses re-entry, so a re-entrant pass returns
        // gracefully without attempting to re-borrow.
        let handles: Vec<Rc<RefCell<dyn LocalMemoryReclaim>>> = self
            .inner
            .borrow()
            .entries
            .iter()
            .map(|entry| Rc::clone(&entry.reclaimer))
            .collect();
        self.coordinator.reclaim_shared(&handles, target_bytes)
    }
}

/// RAII registration handle. Dropping it unregisters the reclaimer from the
/// [`ReclaimRegistry`] it came from, so a retention site's reclaimer is removed
/// automatically when the site is torn down (no explicit unregister call and no
/// dangling reclaimer left behind).
#[must_use = "dropping the registration unregisters the reclaimer"]
pub struct ReclaimRegistration {
    registry: Weak<RefCell<ReclaimRegistryInner>>,
    id: u64,
}

impl Drop for ReclaimRegistration {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            registry
                .borrow_mut()
                .entries
                .retain(|entry| entry.id != self.id);
        }
    }
}

thread_local! {
    /// Per-runtime-thread slot for the current pipeline's reclaim registry.
    ///
    /// Set on the pinned pipeline runtime thread by
    /// [`set_current_reclaim_registry`] and cleared by dropping the returned
    /// [`ReclaimRegistryGuard`]. Mirrors the runtime memory-budget accessor:
    /// reads are intra-thread (`borrow().clone()` is a handful of `Rc`
    /// strong-count bumps with no atomics or shared coordination), so a stateful
    /// retention site running on the runtime thread can reach the single
    /// per-runtime registry to register its reclaimer.
    static CURRENT_RECLAIM_REGISTRY: RefCell<Option<ReclaimRegistry>> =
        const { RefCell::new(None) };
}

/// RAII guard returned by [`set_current_reclaim_registry`].
///
/// On drop, restores the previous (typically `None`) value of the reclaim
/// registry slot for this thread. The guard is `!Send` so it cannot escape the
/// runtime thread it was created on, guaranteeing "the registry set here is also
/// cleared here, on the same thread".
#[must_use = "the reclaim registry remains installed until this guard is dropped"]
pub struct ReclaimRegistryGuard {
    previous: Option<ReclaimRegistry>,
    _not_send: PhantomData<Rc<()>>,
}

impl Drop for ReclaimRegistryGuard {
    fn drop(&mut self) {
        let prev = self.previous.take();
        CURRENT_RECLAIM_REGISTRY.with(|slot: &RefCell<Option<ReclaimRegistry>>| {
            let _ = slot.replace(prev);
        });
    }
}

/// Installs a reclaim registry as the current thread's accessor.
///
/// Returns a guard that, when dropped, restores whatever was installed before
/// this call (typically `None`). Must be called on the pinned pipeline runtime
/// thread; the returned guard is `!Send` and must drop on that same thread.
/// Passing `None` clears the slot.
pub fn set_current_reclaim_registry(registry: Option<ReclaimRegistry>) -> ReclaimRegistryGuard {
    let previous = CURRENT_RECLAIM_REGISTRY
        .with(|slot: &RefCell<Option<ReclaimRegistry>>| slot.replace(registry));
    ReclaimRegistryGuard {
        previous,
        _not_send: PhantomData,
    }
}

/// Returns a clone of the current runtime thread's reclaim registry.
///
/// Returns `None` outside a pipeline runtime thread, before
/// [`set_current_reclaim_registry`] has been called, or after the guard for this
/// runtime has been dropped. The clone shares the registry's `Rc`-backed state,
/// so registering through it affects the single per-runtime registry.
#[must_use]
pub fn current_reclaim_registry() -> Option<ReclaimRegistry> {
    CURRENT_RECLAIM_REGISTRY.with(|slot: &RefCell<Option<ReclaimRegistry>>| slot.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_budget::BudgetScopeId;
    use crate::memory_budget::{
        BudgetMode, GlobalLeasePool, LocalMemoryTicket, RuntimeMemoryAccount, RuntimeMemorySnapshot,
    };
    use std::rc::Rc;
    use std::sync::Arc;

    fn test_account() -> Rc<RuntimeMemoryAccount> {
        Rc::new(RuntimeMemoryAccount::new(
            10_000,
            64,
            128,
            BudgetMode::ObserveOnly,
            Arc::new(GlobalLeasePool::new(0)),
            Arc::new(RuntimeMemorySnapshot::default()),
            Arc::new(BudgetScopeId::default()),
        ))
    }

    /// A reclaimer that releases retained bytes by dropping the local tickets it
    /// already owns. It never acquires budget, demonstrating the budget-free
    /// release contract.
    struct TicketReclaimer {
        priority: ReclaimPriority,
        held: Vec<LocalMemoryTicket>,
        calls: u32,
    }

    impl TicketReclaimer {
        fn new(
            priority: ReclaimPriority,
            account: &Rc<RuntimeMemoryAccount>,
            items: &[u64],
        ) -> Self {
            let held = items
                .iter()
                .map(|&bytes| account.charge(bytes).expect("observe-only charge fits"))
                .collect();
            Self {
                priority,
                held,
                calls: 0,
            }
        }
    }

    impl LocalMemoryReclaim for TicketReclaimer {
        fn reclaim_priority(&self) -> ReclaimPriority {
            self.priority
        }

        fn reclaim(&mut self, target_bytes: u64, _context: ReclaimContext<'_>) -> ReclaimResult {
            self.calls += 1;
            let mut released = 0_u64;
            let mut attempted = 0_u64;
            // Drop one held ticket at a time until the target is satisfied. Drop
            // (RAII) is the only release mechanism; no budget is acquired.
            while released < target_bytes {
                let Some(ticket) = self.held.pop() else {
                    break;
                };
                attempted = attempted.saturating_add(ticket.bytes().unwrap_or(0));
                released = released.saturating_add(ticket.bytes().unwrap_or(0));
                drop(ticket);
            }
            ReclaimResult::released(attempted, released, !self.held.is_empty())
        }
    }

    /// A reclaimer that can never release anything.
    struct StuckReclaimer {
        priority: ReclaimPriority,
    }

    impl LocalMemoryReclaim for StuckReclaimer {
        fn reclaim_priority(&self) -> ReclaimPriority {
            self.priority
        }

        fn reclaim(&mut self, _target_bytes: u64, _context: ReclaimContext<'_>) -> ReclaimResult {
            ReclaimResult::no_progress()
        }
    }

    /// A reclaimer that re-enters the coordinator while it runs, to exercise the
    /// re-entrancy guard.
    struct ReentrantReclaimer {
        coordinator: Rc<ReclaimCoordinator>,
        observed_reentry: bool,
    }

    impl LocalMemoryReclaim for ReentrantReclaimer {
        fn reclaim_priority(&self) -> ReclaimPriority {
            ReclaimPriority::Processor
        }

        fn reclaim(&mut self, _target_bytes: u64, _context: ReclaimContext<'_>) -> ReclaimResult {
            // Attempt a nested pass; it must be refused.
            let nested = self.coordinator.reclaim(&mut [], 1);
            self.observed_reentry = nested.reentered;
            ReclaimResult::no_progress()
        }
    }

    #[test]
    fn reclaim_releases_up_to_target_and_stops() {
        let account = test_account();
        let mut reclaimer = TicketReclaimer::new(ReclaimPriority::Queue, &account, &[40, 40, 40]);
        let charged_before = account.charged_bytes.get();
        assert_eq!(charged_before, 120);

        let coordinator = ReclaimCoordinator::new();
        let outcome = coordinator.reclaim(&mut [&mut reclaimer as &mut dyn LocalMemoryReclaim], 60);

        assert!(outcome.target_met);
        assert_eq!(
            outcome.released_bytes, 80,
            "drops whole tickets to meet target"
        );
        assert!(!outcome.reentered);
        // Released bytes left the budget; nothing was acquired.
        assert_eq!(account.charged_bytes.get(), 40);
        // One ticket remains held.
        assert_eq!(reclaimer.held.len(), 1);
    }

    #[test]
    fn reclaim_reports_partial_progress_when_target_unmet() {
        let account = test_account();
        let mut reclaimer = TicketReclaimer::new(ReclaimPriority::Buffer, &account, &[30, 30]);
        let coordinator = ReclaimCoordinator::new();

        let outcome =
            coordinator.reclaim(&mut [&mut reclaimer as &mut dyn LocalMemoryReclaim], 1_000);

        assert!(!outcome.target_met);
        assert_eq!(outcome.released_bytes, 60);
        assert_eq!(account.charged_bytes.get(), 0);
        assert!(reclaimer.held.is_empty());
    }

    #[test]
    fn reclaim_handles_no_progress_without_spinning() {
        let mut stuck = StuckReclaimer {
            priority: ReclaimPriority::Stream,
        };
        let coordinator = ReclaimCoordinator::new();

        let outcome = coordinator.reclaim(&mut [&mut stuck as &mut dyn LocalMemoryReclaim], 500);

        assert!(!outcome.target_met);
        assert_eq!(outcome.released_bytes, 0);
        assert!(!outcome.reentered);
    }

    #[test]
    fn reclaim_drives_priority_order_queue_before_stream() {
        let account = test_account();
        // Stream reclaimer is registered first but must run after the queue
        // reclaimer; with a target of 10 only the first-asked reclaimer releases.
        let mut stream = TicketReclaimer::new(ReclaimPriority::Stream, &account, &[10]);
        let mut queue = TicketReclaimer::new(ReclaimPriority::Queue, &account, &[10]);
        let coordinator = ReclaimCoordinator::new();

        let outcome = coordinator.reclaim(
            &mut [
                &mut stream as &mut dyn LocalMemoryReclaim,
                &mut queue as &mut dyn LocalMemoryReclaim,
            ],
            10,
        );

        assert!(outcome.target_met);
        assert_eq!(queue.calls, 1, "queue reclaimer is asked first");
        assert_eq!(stream.calls, 0, "stream reclaimer is not reached once met");
    }

    #[test]
    fn reclaim_refuses_reentry() {
        let coordinator = Rc::new(ReclaimCoordinator::new());
        let mut reentrant = ReentrantReclaimer {
            coordinator: Rc::clone(&coordinator),
            observed_reentry: false,
        };

        let outcome =
            coordinator.reclaim(&mut [&mut reentrant as &mut dyn LocalMemoryReclaim], 100);

        assert!(!outcome.reentered, "outer pass runs normally");
        assert!(
            reentrant.observed_reentry,
            "nested reclaim must be refused while one is in progress"
        );
        // Guard is cleared after the pass completes.
        assert!(!coordinator.is_reclaiming());
    }

    // ----------------------------------------------------------------------
    // Registry lifecycle.
    // ----------------------------------------------------------------------

    #[test]
    fn registry_drives_registered_reclaimers_in_priority_order() {
        let account = test_account();
        let registry = ReclaimRegistry::new();
        assert!(registry.is_empty());

        // Register stream first, queue second; priority must drive queue first.
        let stream = Rc::new(RefCell::new(TicketReclaimer::new(
            ReclaimPriority::Stream,
            &account,
            &[10],
        )));
        let queue = Rc::new(RefCell::new(TicketReclaimer::new(
            ReclaimPriority::Queue,
            &account,
            &[10],
        )));
        let _stream_reg = registry.register(stream.clone() as Rc<RefCell<dyn LocalMemoryReclaim>>);
        let _queue_reg = registry.register(queue.clone() as Rc<RefCell<dyn LocalMemoryReclaim>>);
        assert_eq!(registry.len(), 2);

        let outcome = registry.reclaim(10);
        assert!(outcome.target_met);
        assert_eq!(queue.borrow().calls, 1, "queue reclaimer is asked first");
        assert_eq!(
            stream.borrow().calls,
            0,
            "stream reclaimer is not reached once met"
        );
        // Budget-free: released bytes left the account, none acquired.
        assert_eq!(account.charged_bytes.get(), 10);
    }

    #[test]
    fn registry_unregisters_on_registration_drop() {
        let account = test_account();
        let registry = ReclaimRegistry::new();

        let reclaimer = Rc::new(RefCell::new(TicketReclaimer::new(
            ReclaimPriority::Buffer,
            &account,
            &[25],
        )));
        let registration =
            registry.register(reclaimer.clone() as Rc<RefCell<dyn LocalMemoryReclaim>>);
        assert_eq!(registry.len(), 1);

        // Dropping the registration removes the reclaimer; a later pass drives
        // nothing (and so cannot meet a non-zero target).
        drop(registration);
        assert!(registry.is_empty());

        let outcome = registry.reclaim(25);
        assert!(!outcome.target_met);
        assert_eq!(outcome.released_bytes, 0);
        assert_eq!(
            account.charged_bytes.get(),
            25,
            "unregistered reclaimer is not driven, so its bytes stay retained"
        );
    }

    #[test]
    fn registry_reclaim_refuses_reentry_without_double_borrow() {
        // A reclaimer that calls back into the same registry mid-pass must be
        // refused gracefully (no BorrowMutError panic from re-borrowing).
        struct RegistryReentrantReclaimer {
            registry: ReclaimRegistry,
            observed_reentry: bool,
        }

        impl LocalMemoryReclaim for RegistryReentrantReclaimer {
            fn reclaim_priority(&self) -> ReclaimPriority {
                ReclaimPriority::Processor
            }

            fn reclaim(
                &mut self,
                _target_bytes: u64,
                _context: ReclaimContext<'_>,
            ) -> ReclaimResult {
                let nested = self.registry.reclaim(1);
                self.observed_reentry = nested.reentered;
                ReclaimResult::no_progress()
            }
        }

        let registry = ReclaimRegistry::new();
        let reclaimer = Rc::new(RefCell::new(RegistryReentrantReclaimer {
            registry: registry.clone(),
            observed_reentry: false,
        }));
        let _reg = registry.register(reclaimer.clone() as Rc<RefCell<dyn LocalMemoryReclaim>>);

        let outcome = registry.reclaim(100);
        assert!(!outcome.reentered, "outer pass runs normally");
        assert!(
            reclaimer.borrow().observed_reentry,
            "nested registry reclaim must be refused while a pass is in progress"
        );
        assert!(!registry.is_reclaiming());
    }

    // ----------------------------------------------------------------------
    // Runtime-thread accessor.
    // ----------------------------------------------------------------------

    #[test]
    fn current_reclaim_registry_absent_by_default() {
        assert!(
            current_reclaim_registry().is_none(),
            "no registry is installed outside a runtime thread setup"
        );
    }

    #[test]
    fn set_and_clear_current_reclaim_registry() {
        let registry = ReclaimRegistry::new();
        let account = test_account();
        let reclaimer = Rc::new(RefCell::new(TicketReclaimer::new(
            ReclaimPriority::Queue,
            &account,
            &[10],
        )));

        {
            let _guard = set_current_reclaim_registry(Some(registry.clone()));
            let current = current_reclaim_registry().expect("registry installed");
            // The accessor returns a handle to the same registry: registering
            // through it is visible on the original handle.
            let _reg = current.register(reclaimer.clone() as Rc<RefCell<dyn LocalMemoryReclaim>>);
            assert_eq!(registry.len(), 1, "registration via the accessor is shared");
        }

        assert!(
            current_reclaim_registry().is_none(),
            "dropping the guard clears the thread accessor"
        );
    }

    #[test]
    fn set_current_reclaim_registry_restores_previous_on_drop() {
        let first = ReclaimRegistry::new();
        let second = ReclaimRegistry::new();
        let account = test_account();
        let r = Rc::new(RefCell::new(TicketReclaimer::new(
            ReclaimPriority::Queue,
            &account,
            &[10],
        )));
        let _first_reg = first.register(r.clone() as Rc<RefCell<dyn LocalMemoryReclaim>>);

        let _outer = set_current_reclaim_registry(Some(first.clone()));
        {
            let _inner = set_current_reclaim_registry(Some(second.clone()));
            assert_eq!(
                current_reclaim_registry().expect("inner installed").len(),
                0,
                "inner registry is the empty second one"
            );
        }
        // Dropping the inner guard restores the outer registry.
        assert_eq!(
            current_reclaim_registry().expect("outer restored").len(),
            1,
            "previous registry is restored after the inner guard drops"
        );
    }

    #[test]
    fn reclaim_registry_guard_is_not_send() {
        static_assertions::assert_not_impl_any!(ReclaimRegistryGuard: Send);
        static_assertions::assert_not_impl_any!(ReclaimRegistry: Send);
    }
}
