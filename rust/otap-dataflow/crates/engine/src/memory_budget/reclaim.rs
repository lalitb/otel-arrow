// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Reclaim-hook foundation (design "Reclaim Hooks", Phase 2e).
//!
//! Stateful retention sites (batch processors, retry buffers, durable buffers,
//! topic queues, stream state, delayed scheduler payloads) can release retained
//! memory on demand when a runtime is under pressure. This module provides the
//! engine-side primitives for that, intentionally scoped to the *mechanism*:
//!
//! - [`LocalMemoryReclaim`]: the `!Send` reclaim trait a stateful component
//!   implements.
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
//!
//! This is the Phase 2e *foundation*: the coordinator does not own a registry
//! of retention sites and is not yet wired into receiver admission or any node.
//! Wiring concrete reclaimers (and enabling reclaim-driven enforcement) is
//! gated behind `enforcement.reclaim_hooks`, which the config layer still
//! rejects. Keeping the primitive separate lets it be reviewed and tested in
//! isolation before any site depends on it.

use std::cell::Cell;
use std::marker::PhantomData;

use async_trait::async_trait;

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
/// Implementations are `!Send` (`?Send` trait): they run on the pinned pipeline
/// runtime thread alongside the local budget account. `reclaim` must be
/// best-effort, bounded, and budget-free — it may drop, drain, abort, or report
/// no progress, but it must not acquire budget. It must also tolerate being
/// called again later.
#[async_trait(?Send)]
pub trait LocalMemoryReclaim {
    /// The ordering class for this reclaimer.
    fn reclaim_priority(&self) -> ReclaimPriority;

    /// Attempts to release up to `target_bytes`. Returns how much was attempted
    /// and released and whether more is available.
    async fn reclaim(&mut self, target_bytes: u64, context: ReclaimContext<'_>) -> ReclaimResult;
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
    pub async fn reclaim(
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
                let result = reclaimers[index].reclaim(remaining, context).await;
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

    #[async_trait(?Send)]
    impl LocalMemoryReclaim for TicketReclaimer {
        fn reclaim_priority(&self) -> ReclaimPriority {
            self.priority
        }

        async fn reclaim(
            &mut self,
            target_bytes: u64,
            _context: ReclaimContext<'_>,
        ) -> ReclaimResult {
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

    #[async_trait(?Send)]
    impl LocalMemoryReclaim for StuckReclaimer {
        fn reclaim_priority(&self) -> ReclaimPriority {
            self.priority
        }

        async fn reclaim(
            &mut self,
            _target_bytes: u64,
            _context: ReclaimContext<'_>,
        ) -> ReclaimResult {
            ReclaimResult::no_progress()
        }
    }

    /// A reclaimer that re-enters the coordinator while it runs, to exercise the
    /// re-entrancy guard.
    struct ReentrantReclaimer {
        coordinator: Rc<ReclaimCoordinator>,
        observed_reentry: bool,
    }

    #[async_trait(?Send)]
    impl LocalMemoryReclaim for ReentrantReclaimer {
        fn reclaim_priority(&self) -> ReclaimPriority {
            ReclaimPriority::Processor
        }

        async fn reclaim(
            &mut self,
            _target_bytes: u64,
            _context: ReclaimContext<'_>,
        ) -> ReclaimResult {
            // Attempt a nested pass; it must be refused.
            let nested = self.coordinator.reclaim(&mut [], 1).await;
            self.observed_reentry = nested.reentered;
            ReclaimResult::no_progress()
        }
    }

    #[tokio::test]
    async fn reclaim_releases_up_to_target_and_stops() {
        let account = test_account();
        let mut reclaimer = TicketReclaimer::new(ReclaimPriority::Queue, &account, &[40, 40, 40]);
        let charged_before = account.charged_bytes.get();
        assert_eq!(charged_before, 120);

        let coordinator = ReclaimCoordinator::new();
        let outcome = coordinator
            .reclaim(&mut [&mut reclaimer as &mut dyn LocalMemoryReclaim], 60)
            .await;

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

    #[tokio::test]
    async fn reclaim_reports_partial_progress_when_target_unmet() {
        let account = test_account();
        let mut reclaimer = TicketReclaimer::new(ReclaimPriority::Buffer, &account, &[30, 30]);
        let coordinator = ReclaimCoordinator::new();

        let outcome = coordinator
            .reclaim(&mut [&mut reclaimer as &mut dyn LocalMemoryReclaim], 1_000)
            .await;

        assert!(!outcome.target_met);
        assert_eq!(outcome.released_bytes, 60);
        assert_eq!(account.charged_bytes.get(), 0);
        assert!(reclaimer.held.is_empty());
    }

    #[tokio::test]
    async fn reclaim_handles_no_progress_without_spinning() {
        let mut stuck = StuckReclaimer {
            priority: ReclaimPriority::Stream,
        };
        let coordinator = ReclaimCoordinator::new();

        let outcome = coordinator
            .reclaim(&mut [&mut stuck as &mut dyn LocalMemoryReclaim], 500)
            .await;

        assert!(!outcome.target_met);
        assert_eq!(outcome.released_bytes, 0);
        assert!(!outcome.reentered);
    }

    #[tokio::test]
    async fn reclaim_drives_priority_order_queue_before_stream() {
        let account = test_account();
        // Stream reclaimer is registered first but must run after the queue
        // reclaimer; with a target of 10 only the first-asked reclaimer releases.
        let mut stream = TicketReclaimer::new(ReclaimPriority::Stream, &account, &[10]);
        let mut queue = TicketReclaimer::new(ReclaimPriority::Queue, &account, &[10]);
        let coordinator = ReclaimCoordinator::new();

        let outcome = coordinator
            .reclaim(
                &mut [
                    &mut stream as &mut dyn LocalMemoryReclaim,
                    &mut queue as &mut dyn LocalMemoryReclaim,
                ],
                10,
            )
            .await;

        assert!(outcome.target_met);
        assert_eq!(queue.calls, 1, "queue reclaimer is asked first");
        assert_eq!(stream.calls, 0, "stream reclaimer is not reached once met");
    }

    #[tokio::test]
    async fn reclaim_refuses_reentry() {
        let coordinator = Rc::new(ReclaimCoordinator::new());
        let mut reentrant = ReentrantReclaimer {
            coordinator: Rc::clone(&coordinator),
            observed_reentry: false,
        };

        let outcome = coordinator
            .reclaim(&mut [&mut reentrant as &mut dyn LocalMemoryReclaim], 100)
            .await;

        assert!(!outcome.reentered, "outer pass runs normally");
        assert!(
            reentrant.observed_reentry,
            "nested reclaim must be refused while one is in progress"
        );
        // Guard is cleared after the pass completes.
        assert!(!coordinator.is_reclaiming());
    }
}
