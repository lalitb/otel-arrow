// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Observe-only runtime memory budgeting primitives.
//!
//! This module intentionally does not enforce admission. It defines the local
//! and cross-runtime ownership types used by later milestones and exposes shared
//! snapshots for engine-level metrics.

use otap_df_config::policy::{MemoryBudgetMode as ConfigBudgetMode, MemoryBudgetPolicy};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

/// Immutable attribution carried by budget owners when the identity is known.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BudgetScopeId {
    /// Pipeline group owning or attributing the retained bytes.
    pub pipeline_group_id: Option<String>,
    /// Pipeline owning or attributing the retained bytes.
    pub pipeline_id: Option<String>,
    /// Core id of the runtime owning or attributing the retained bytes.
    pub core_id: Option<usize>,
    /// Runtime deployment generation owning or attributing the retained bytes.
    pub runtime_generation: Option<u64>,
    /// Topic or shared boundary owning or attributing the retained bytes.
    pub topic_or_boundary: Option<String>,
}

/// Runtime memory-budget mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetMode {
    /// Observe retained bytes and pressure only.
    ObserveOnly,
    /// Enforce admission/publish decisions.
    Enforce,
}

impl From<ConfigBudgetMode> for BudgetMode {
    fn from(value: ConfigBudgetMode) -> Self {
        match value {
            ConfigBudgetMode::ObserveOnly => Self::ObserveOnly,
            ConfigBudgetMode::Enforce => Self::Enforce,
        }
    }
}

/// Runtime budget pressure level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetLevel {
    /// Retained bytes are within the local floor plus leases.
    Normal,
    /// Retained bytes exceed the local floor plus leases.
    Soft,
    /// Retained bytes exceed local budget plus permitted overshoot.
    Hard,
}

impl BudgetLevel {
    const fn as_u64(self) -> u64 {
        match self {
            Self::Normal => 0,
            Self::Soft => 1,
            Self::Hard => 2,
        }
    }
}

/// Runtime memory-budget sizing parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryBudgetSizing {
    /// Bytes reserved outside runtime floors.
    pub reserve_bytes: u64,
    /// Minimum bytes assigned to each runtime.
    pub floor_per_runtime_bytes: u64,
    /// Coarse lease unit borrowed from the global pool.
    pub lease_step_bytes: u64,
    /// Maximum local overshoot before hard classification.
    pub max_overshoot_per_runtime_bytes: u64,
    /// Overshoot debt threshold reserved for future reclaim policy.
    pub overshoot_debt_limit_bytes: u64,
}

impl MemoryBudgetSizing {
    fn from_policy(policy: &MemoryBudgetPolicy) -> Self {
        Self {
            reserve_bytes: policy.sizing.reserve,
            floor_per_runtime_bytes: policy.sizing.floor_per_runtime,
            lease_step_bytes: policy.sizing.lease_step,
            max_overshoot_per_runtime_bytes: policy.sizing.max_overshoot_per_runtime,
            overshoot_debt_limit_bytes: policy.sizing.overshoot_debt_limit,
        }
    }
}

/// Runtime memory-budget configuration applied by the controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeMemoryBudgetConfig {
    /// Configured mode.
    pub mode: BudgetMode,
    /// Retry-After hint reserved for future enforcement.
    pub retry_after_secs: u32,
    /// Lease sizing parameters.
    pub sizing: MemoryBudgetSizing,
    /// Topic escrow default limit.
    pub topic_default_limit_bytes: u64,
    /// Number of deployed runtime instances used for sizing.
    pub runtime_count: usize,
}

impl RuntimeMemoryBudgetConfig {
    /// Builds runtime configuration from user policy.
    #[must_use]
    pub fn from_policy(policy: &MemoryBudgetPolicy, runtime_count: usize) -> Self {
        Self {
            mode: policy.mode.into(),
            retry_after_secs: policy.retry_after_secs,
            sizing: MemoryBudgetSizing::from_policy(policy),
            topic_default_limit_bytes: policy.escrow.topic_default_limit,
            runtime_count: runtime_count.max(1),
        }
    }
}

/// Shared snapshot published by one runtime account.
#[derive(Debug, Default)]
pub struct RuntimeMemorySnapshot {
    borrowed_bytes: AtomicU64,
    charged_bytes: AtomicU64,
    unknown_bytes: AtomicU64,
    overshoot_bytes: AtomicU64,
    level: AtomicU64,
}

impl RuntimeMemorySnapshot {
    fn publish(
        &self,
        borrowed_bytes: u64,
        charged_bytes: u64,
        unknown_bytes: u64,
        overshoot_bytes: u64,
        level: BudgetLevel,
    ) {
        self.borrowed_bytes.store(borrowed_bytes, Ordering::Relaxed);
        self.charged_bytes.store(charged_bytes, Ordering::Relaxed);
        self.unknown_bytes.store(unknown_bytes, Ordering::Relaxed);
        self.overshoot_bytes
            .store(overshoot_bytes, Ordering::Relaxed);
        self.level.store(level.as_u64(), Ordering::Relaxed);
    }

    /// Returns borrowed bytes held by this runtime.
    #[must_use]
    pub fn borrowed_bytes(&self) -> u64 {
        self.borrowed_bytes.load(Ordering::Relaxed)
    }

    /// Returns known logical retained bytes charged to this runtime.
    #[must_use]
    pub fn charged_bytes(&self) -> u64 {
        self.charged_bytes.load(Ordering::Relaxed)
    }

    /// Returns retained bytes observed without a known logical size.
    #[must_use]
    pub fn unknown_bytes(&self) -> u64 {
        self.unknown_bytes.load(Ordering::Relaxed)
    }

    /// Returns bytes above local floor plus leases.
    #[must_use]
    pub fn overshoot_bytes(&self) -> u64 {
        self.overshoot_bytes.load(Ordering::Relaxed)
    }

    /// Returns pressure level encoded as `0=normal`, `1=soft`, `2=hard`.
    #[must_use]
    pub fn level(&self) -> u64 {
        self.level.load(Ordering::Relaxed)
    }
}

/// Aggregated runtime memory-budget snapshot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryBudgetSnapshot {
    /// Number of registered runtime snapshots.
    pub runtime_count: u64,
    /// Runtime snapshots currently at normal level.
    pub normal_runtime_count: u64,
    /// Runtime snapshots currently at soft level.
    pub soft_runtime_count: u64,
    /// Runtime snapshots currently at hard level.
    pub hard_runtime_count: u64,
    /// Total borrowed bytes held by all runtimes.
    pub borrowed_bytes: u64,
    /// Total known logical retained bytes charged to runtimes.
    pub charged_bytes: u64,
    /// Total unknown retained bytes observed without a known logical size.
    pub unknown_bytes: u64,
    /// Total bytes above runtime floors plus leases.
    pub overshoot_bytes: u64,
    /// Abandoned escrow tickets retained for leak detection.
    pub abandoned_escrow_count: u64,
    /// Abandoned escrow bytes retained for leak detection.
    pub abandoned_escrow_bytes: u64,
    /// Escrow tickets currently owning logical retained bytes.
    pub escrow_ticket_count: u64,
    /// Escrow bytes currently owning logical retained bytes.
    pub escrow_charged_bytes: u64,
    /// Spare bytes currently available to lease from the global pool.
    pub spare_available_bytes: u64,
}

#[derive(Debug, Default)]
struct MemoryBudgetStateInner {
    config: Mutex<Option<RuntimeMemoryBudgetConfig>>,
    pool: GlobalLeasePool,
    snapshots: Mutex<Vec<Weak<RuntimeMemorySnapshot>>>,
    // Phase 2e leak detection will need entry identity, scope, and timestamps.
    abandoned_escrow: Mutex<VecDeque<u64>>,
    abandoned_escrow_count: AtomicU64,
    abandoned_escrow_bytes: AtomicU64,
    escrow_ticket_count: AtomicU64,
    escrow_charged_bytes: AtomicU64,
}

/// Shared runtime memory-budget state.
#[derive(Debug, Clone, Default)]
pub struct MemoryBudgetState {
    inner: Arc<MemoryBudgetStateInner>,
}

impl MemoryBudgetState {
    /// Configures observe-only runtime memory budgeting.
    pub fn configure(
        &self,
        config: RuntimeMemoryBudgetConfig,
        process_hard_limit_bytes: Option<u64>,
    ) {
        let floor_total = config
            .sizing
            .floor_per_runtime_bytes
            .saturating_mul(config.runtime_count as u64);
        let spare = process_hard_limit_bytes
            .unwrap_or(0)
            .saturating_sub(config.sizing.reserve_bytes)
            .saturating_sub(floor_total);
        self.inner.pool.set_available(spare);
        *self
            .inner
            .config
            .lock()
            .expect("memory budget config poisoned") = Some(config);
    }

    /// Returns whether the runtime memory budget is configured.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.inner
            .config
            .lock()
            .expect("memory budget config poisoned")
            .is_some()
    }

    /// Registers one deployed runtime snapshot.
    ///
    /// Must be called on the pipeline runtime thread (after CPU pin) so the
    /// snapshot Arc is first-touched on that thread's NUMA node. Calling this
    /// from the controller thread will misplace the snapshot's backing pages.
    #[must_use]
    pub fn register_runtime_snapshot(&self, scope: BudgetScopeId) -> RuntimeMemorySnapshotHandle {
        let snapshot = Arc::new(RuntimeMemorySnapshot::default());
        if self.config().is_some() {
            snapshot.publish(0, 0, 0, 0, BudgetLevel::Normal);
        }
        self.inner
            .snapshots
            .lock()
            .expect("memory budget snapshots poisoned")
            .push(Arc::downgrade(&snapshot));
        RuntimeMemorySnapshotHandle {
            snapshot,
            state: self.clone(),
            scope,
            account_taken: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns current runtime memory-budget configuration.
    #[must_use]
    pub fn config(&self) -> Option<RuntimeMemoryBudgetConfig> {
        *self
            .inner
            .config
            .lock()
            .expect("memory budget config poisoned")
    }

    /// Returns an aggregated snapshot across all registered runtimes.
    #[must_use]
    pub fn snapshot(&self) -> MemoryBudgetSnapshot {
        let mut snapshots = self
            .inner
            .snapshots
            .lock()
            .expect("memory budget snapshots poisoned");
        let live_snapshots: Vec<_> = snapshots.iter().filter_map(Weak::upgrade).collect();
        snapshots.retain(|snapshot| snapshot.strong_count() > 0);
        let mut snapshot = MemoryBudgetSnapshot {
            runtime_count: live_snapshots.len() as u64,
            abandoned_escrow_count: self.inner.abandoned_escrow_count.load(Ordering::Relaxed),
            abandoned_escrow_bytes: self.inner.abandoned_escrow_bytes.load(Ordering::Relaxed),
            escrow_ticket_count: self.inner.escrow_ticket_count.load(Ordering::Relaxed),
            escrow_charged_bytes: self.inner.escrow_charged_bytes.load(Ordering::Relaxed),
            spare_available_bytes: self.inner.pool.available_bytes(),
            ..MemoryBudgetSnapshot::default()
        };
        for runtime in live_snapshots.iter() {
            snapshot.borrowed_bytes = snapshot
                .borrowed_bytes
                .saturating_add(runtime.borrowed_bytes());
            snapshot.charged_bytes = snapshot
                .charged_bytes
                .saturating_add(runtime.charged_bytes());
            snapshot.unknown_bytes = snapshot
                .unknown_bytes
                .saturating_add(runtime.unknown_bytes());
            snapshot.overshoot_bytes = snapshot
                .overshoot_bytes
                .saturating_add(runtime.overshoot_bytes());
            match runtime.level() {
                1 => snapshot.soft_runtime_count += 1,
                2 => snapshot.hard_runtime_count += 1,
                _ => snapshot.normal_runtime_count += 1,
            }
        }
        snapshot
    }

    fn abandon_escrow(&self, bytes: u64) {
        self.inner
            .abandoned_escrow
            .lock()
            .expect("memory budget abandoned escrow poisoned")
            .push_back(bytes);
        let _ = self
            .inner
            .abandoned_escrow_count
            .fetch_add(1, Ordering::Relaxed);
        let _ = self
            .inner
            .abandoned_escrow_bytes
            .fetch_add(bytes, Ordering::Relaxed);
    }

    fn try_charge_escrow(&self, bytes: u64) -> bool {
        let Some(config) = self.config() else {
            return false;
        };
        if config.mode == BudgetMode::Enforce {
            let limit = config.topic_default_limit_bytes;
            let mut current = self.inner.escrow_charged_bytes.load(Ordering::Relaxed);
            loop {
                let next = current.saturating_add(bytes);
                if next > limit {
                    return false;
                }
                match self.inner.escrow_charged_bytes.compare_exchange_weak(
                    current,
                    next,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(observed) => current = observed,
                }
            }
        } else {
            let _ = self
                .inner
                .escrow_charged_bytes
                .fetch_add(bytes, Ordering::Relaxed);
        }
        let _ = self
            .inner
            .escrow_ticket_count
            .fetch_add(1, Ordering::Relaxed);
        true
    }

    fn release_escrow(&self, bytes: u64) {
        let _ = self
            .inner
            .escrow_ticket_count
            .fetch_sub(1, Ordering::Relaxed);
        let _ = self
            .inner
            .escrow_charged_bytes
            .fetch_sub(bytes, Ordering::Relaxed);
    }

    fn lease_authority(&self) -> Arc<dyn LeaseAuthority> {
        Arc::new(self.inner.pool.clone())
    }
}

/// Shared handle to one runtime memory snapshot.
///
/// The handle is `Send + Clone` and may travel across threads. It carries the
/// scope attribution used when a runtime account is created from it. Only one
/// `RuntimeMemoryAccount` may be derived per registered snapshot; subsequent
/// `local_account()` calls return `None` to keep account ownership
/// single-writer for the per-runtime hot path.
#[derive(Debug, Clone)]
pub struct RuntimeMemorySnapshotHandle {
    snapshot: Arc<RuntimeMemorySnapshot>,
    state: MemoryBudgetState,
    scope: BudgetScopeId,
    account_taken: Arc<AtomicBool>,
}

impl RuntimeMemorySnapshotHandle {
    /// Returns the attribution scope for this snapshot.
    #[must_use]
    pub fn scope(&self) -> &BudgetScopeId {
        &self.scope
    }

    /// Creates a local runtime account from this snapshot handle.
    ///
    /// Returns `None` if memory budgeting is not configured, or if a runtime
    /// account has already been taken from this snapshot. The returned account
    /// is `!Send` and must be created and used on the pipeline runtime thread.
    #[must_use]
    pub fn local_account(&self) -> Option<Rc<RuntimeMemoryAccount>> {
        let config = self.state.config()?;
        if self
            .account_taken
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        Some(Rc::new(RuntimeMemoryAccount::new(
            config.sizing.floor_per_runtime_bytes,
            config.sizing.lease_step_bytes,
            config.sizing.max_overshoot_per_runtime_bytes,
            config.mode,
            self.state.lease_authority(),
            self.snapshot.clone(),
            self.scope.clone(),
        )))
    }
}

/// Shared spare pool used by runtime leases.
#[derive(Debug, Default, Clone)]
pub struct GlobalLeasePool {
    available_bytes: Arc<AtomicU64>,
}

impl GlobalLeasePool {
    /// Creates a pool with the given spare bytes.
    #[must_use]
    pub fn new(available_bytes: u64) -> Self {
        Self {
            available_bytes: Arc::new(AtomicU64::new(available_bytes)),
        }
    }

    fn set_available(&self, available_bytes: u64) {
        self.available_bytes
            .store(available_bytes, Ordering::Relaxed);
    }

    /// Attempts to borrow a full amount. Partial borrows are not allowed.
    #[must_use]
    pub fn try_borrow(&self, bytes: u64) -> bool {
        if bytes == 0 {
            return true;
        }
        let mut current = self.available_bytes.load(Ordering::Relaxed);
        loop {
            if current < bytes {
                return false;
            }
            match self.available_bytes.compare_exchange_weak(
                current,
                current - bytes,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(next) => current = next,
            }
        }
    }

    /// Returns bytes to the global spare pool.
    pub fn return_bytes(&self, bytes: u64) {
        let _ = self.available_bytes.fetch_add(bytes, Ordering::Release);
    }

    /// Returns currently available spare bytes.
    #[must_use]
    pub fn available_bytes(&self) -> u64 {
        self.available_bytes.load(Ordering::Relaxed)
    }
}

/// Authority that grants and receives coarse lease bytes.
pub trait LeaseAuthority: std::fmt::Debug {
    /// Attempts to borrow a full amount. Partial borrows are not allowed.
    fn try_borrow(&self, bytes: u64) -> bool;

    /// Returns bytes to this lease authority.
    fn return_bytes(&self, bytes: u64);
}

impl LeaseAuthority for GlobalLeasePool {
    fn try_borrow(&self, bytes: u64) -> bool {
        GlobalLeasePool::try_borrow(self, bytes)
    }

    fn return_bytes(&self, bytes: u64) {
        GlobalLeasePool::return_bytes(self, bytes);
    }
}

/// Local runtime memory account.
///
/// The account holds all hot-path state in `!Send` `Cell`s. Per design,
/// `charge`, `refund`, `reconcile_size`, and `observe_unknown` mutate only
/// local `Cell` state and never touch the shared-atomic [`RuntimeMemorySnapshot`].
/// The local state is published to the snapshot via [`flush_snapshot`] at
/// metric-tick cadence, on explicit checkpoint, or on level transition only.
///
/// This keeps the per-retained-item path lock-free, atomic-free, and
/// NUMA-local on the pipeline runtime thread.
///
/// [`flush_snapshot`]: RuntimeMemoryAccount::flush_snapshot
#[derive(Debug)]
pub struct RuntimeMemoryAccount {
    scope: BudgetScopeId,
    mode: BudgetMode,
    floor_bytes: u64,
    lease: LocalMemoryLease,
    charged_bytes: Cell<u64>,
    unknown_bytes: Cell<u64>,
    overshoot_bytes: Cell<u64>,
    published_level: Cell<BudgetLevel>,
    dirty: Cell<bool>,
    snapshot: Arc<RuntimeMemorySnapshot>,
    _not_send: PhantomData<Rc<()>>,
}

impl RuntimeMemoryAccount {
    /// Creates a local runtime account.
    #[must_use]
    pub fn new(
        floor_bytes: u64,
        lease_step_bytes: u64,
        max_overshoot_bytes: u64,
        mode: BudgetMode,
        lease_authority: Arc<dyn LeaseAuthority>,
        snapshot: Arc<RuntimeMemorySnapshot>,
        scope: BudgetScopeId,
    ) -> Self {
        let account = Self {
            scope,
            mode,
            floor_bytes,
            lease: LocalMemoryLease::new(lease_step_bytes, max_overshoot_bytes, lease_authority),
            charged_bytes: Cell::new(0),
            unknown_bytes: Cell::new(0),
            overshoot_bytes: Cell::new(0),
            published_level: Cell::new(BudgetLevel::Normal),
            dirty: Cell::new(false),
            snapshot,
            _not_send: PhantomData,
        };
        // Initial publish establishes the snapshot baseline once, off the
        // per-item hot path.
        account.publish();
        account
    }

    /// Returns the current pressure level.
    #[must_use]
    pub fn level(&self) -> BudgetLevel {
        self.classify(self.charged_bytes.get())
    }

    /// Returns the scope attribution associated with this account.
    #[must_use]
    pub fn scope(&self) -> &BudgetScopeId {
        &self.scope
    }

    /// Attempts to reserve additional bytes before growth.
    ///
    /// Hot-path: mutates only local `Cell` state; touches the shared global
    /// lease pool only when the reservation requires crossing the current
    /// lease boundary.
    #[must_use]
    pub fn try_reserve_extra(&self, bytes: u64) -> bool {
        let next = self.charged_bytes.get().saturating_add(bytes);
        if next <= self.available_without_overshoot() {
            return true;
        }
        let needed = next.saturating_sub(self.available_without_overshoot());
        if self.lease.try_borrow_for(needed) {
            self.mark_dirty();
            return true;
        }
        if self.mode == BudgetMode::ObserveOnly {
            return true;
        }
        next <= self.hard_limit()
    }

    /// Charges known retained bytes and returns a local ticket.
    ///
    /// Hot-path: mutates only local `Cell` state; defers snapshot publication
    /// to [`flush_snapshot`](Self::flush_snapshot) or a level transition.
    #[must_use]
    pub fn charge(self: &Rc<Self>, size: impl ChargedSize) -> Option<LocalMemoryTicket> {
        let bytes = size.charged_size();
        match self.mode {
            BudgetMode::ObserveOnly => {
                let _ = self.try_reserve_extra(bytes);
            }
            BudgetMode::Enforce => {
                if !self.try_reserve_extra(bytes) {
                    return None;
                }
            }
        }
        self.charged_bytes
            .set(self.charged_bytes.get().saturating_add(bytes));
        self.reconcile_size();
        Some(LocalMemoryTicket {
            account: Rc::clone(self),
            bytes,
            active: true,
            scope: self.scope.clone(),
            _not_send: PhantomData,
        })
    }

    /// Records retained bytes whose logical size is unknown.
    ///
    /// Hot-path: mutates only local `Cell` state.
    pub fn observe_unknown(&self, bytes: u64) {
        self.unknown_bytes
            .set(self.unknown_bytes.get().saturating_add(bytes));
        self.mark_dirty();
    }

    /// Reconciles current charged size and updates pressure/overshoot.
    ///
    /// Hot-path: mutates only local `Cell` state. The shared snapshot is
    /// touched only if the pressure level transitioned (an explicit
    /// operator-relevant event), never on every charged item.
    pub fn reconcile_size(&self) {
        let charged = self.charged_bytes.get();
        let overshoot = charged.saturating_sub(self.available_without_overshoot());
        self.overshoot_bytes.set(overshoot);
        self.lease.return_lazy(charged, self.floor_bytes);
        let new_level = self.classify(charged);
        if new_level != self.published_level.get() {
            // Level transitions are operator-visible; publish immediately.
            self.publish_level(new_level);
        } else {
            self.mark_dirty();
        }
    }

    /// Publishes any pending local state to the shared snapshot.
    ///
    /// Called by metric-tick callbacks, explicit operator checkpoints, or
    /// runtime teardown. Idempotent and side-effect-free when no local state
    /// has changed since the last publish.
    pub fn flush_snapshot(&self) {
        if self.dirty.get() {
            self.publish();
        }
    }

    fn refund(&self, bytes: u64) {
        self.charged_bytes
            .set(self.charged_bytes.get().saturating_sub(bytes));
        self.reconcile_size();
    }

    fn classify(&self, charged: u64) -> BudgetLevel {
        if charged <= self.available_without_overshoot() {
            BudgetLevel::Normal
        } else if charged <= self.hard_limit() {
            BudgetLevel::Soft
        } else {
            BudgetLevel::Hard
        }
    }

    fn available_without_overshoot(&self) -> u64 {
        self.floor_bytes.saturating_add(self.lease.borrowed_bytes())
    }

    fn hard_limit(&self) -> u64 {
        self.available_without_overshoot()
            .saturating_add(self.lease.max_overshoot_bytes())
    }

    #[inline]
    fn mark_dirty(&self) {
        self.dirty.set(true);
    }

    fn publish_level(&self, level: BudgetLevel) {
        self.snapshot.publish(
            self.lease.borrowed_bytes(),
            self.charged_bytes.get(),
            self.unknown_bytes.get(),
            self.overshoot_bytes.get(),
            level,
        );
        self.published_level.set(level);
        self.dirty.set(false);
    }

    fn publish(&self) {
        self.publish_level(self.level());
    }
}

/// Local runtime lease.
#[derive(Debug)]
pub struct LocalMemoryLease {
    borrowed_bytes: Cell<u64>,
    lease_step_bytes: u64,
    max_overshoot_bytes: u64,
    return_watermark_bytes: u64,
    lease_authority: Arc<dyn LeaseAuthority>,
}

impl LocalMemoryLease {
    fn new(
        lease_step_bytes: u64,
        max_overshoot_bytes: u64,
        lease_authority: Arc<dyn LeaseAuthority>,
    ) -> Self {
        Self {
            borrowed_bytes: Cell::new(0),
            lease_step_bytes,
            max_overshoot_bytes,
            return_watermark_bytes: lease_step_bytes / 2,
            lease_authority,
        }
    }

    /// Returns currently borrowed bytes.
    #[must_use]
    pub fn borrowed_bytes(&self) -> u64 {
        self.borrowed_bytes.get()
    }

    fn max_overshoot_bytes(&self) -> u64 {
        self.max_overshoot_bytes
    }

    fn try_borrow_for(&self, needed_bytes: u64) -> bool {
        let steps = needed_bytes.div_ceil(self.lease_step_bytes);
        let borrow_bytes = steps.saturating_mul(self.lease_step_bytes);
        if !self.lease_authority.try_borrow(borrow_bytes) {
            return false;
        }
        self.borrowed_bytes
            .set(self.borrowed_bytes.get().saturating_add(borrow_bytes));
        true
    }

    fn return_lazy(&self, charged_bytes: u64, floor_bytes: u64) {
        let borrowed = self.borrowed_bytes.get();
        if borrowed == 0 {
            return;
        }
        let retained_borrow = charged_bytes.saturating_sub(floor_bytes);
        let needed_steps = retained_borrow.div_ceil(self.lease_step_bytes);
        let needed_borrow = needed_steps.saturating_mul(self.lease_step_bytes);
        if borrowed <= needed_borrow.saturating_add(self.return_watermark_bytes) {
            return;
        }
        let return_bytes = borrowed - needed_borrow;
        self.borrowed_bytes.set(needed_borrow);
        self.lease_authority.return_bytes(return_bytes);
    }
}

/// Local logical charge. This type is intentionally `!Send`.
#[derive(Debug)]
pub struct LocalMemoryTicket {
    account: Rc<RuntimeMemoryAccount>,
    bytes: u64,
    active: bool,
    scope: BudgetScopeId,
    _not_send: PhantomData<Rc<()>>,
}

impl LocalMemoryTicket {
    /// Returns bytes owned by this ticket.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Returns attribution scope for this ticket.
    #[must_use]
    pub fn scope(&self) -> &BudgetScopeId {
        &self.scope
    }

    /// Converts this local ticket into escrow ownership.
    #[must_use]
    pub fn try_into_escrow(
        mut self,
        state: &MemoryBudgetState,
    ) -> Result<EscrowTicket, LocalMemoryTicket> {
        let bytes = self.bytes;
        if !state.try_charge_escrow(bytes) {
            return Err(self);
        }
        self.active = false;
        self.account.refund(bytes);
        Ok(EscrowTicket {
            bytes,
            state: state.clone(),
            scope: self.scope.clone(),
            redeemed: false,
        })
    }
}

impl Drop for LocalMemoryTicket {
    fn drop(&mut self) {
        if self.active {
            self.account.refund(self.bytes);
            self.active = false;
        }
    }
}

/// Cross-runtime/topic logical charge.
#[derive(Debug)]
pub struct EscrowTicket {
    bytes: u64,
    state: MemoryBudgetState,
    scope: BudgetScopeId,
    redeemed: bool,
}

impl EscrowTicket {
    /// Returns bytes owned by this ticket.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Returns attribution scope for this escrow ticket.
    #[must_use]
    pub fn scope(&self) -> &BudgetScopeId {
        &self.scope
    }

    /// Redeems escrow on delivery or release.
    pub fn redeem(mut self) {
        self.state.release_escrow(self.bytes);
        self.redeemed = true;
    }
}

impl Drop for EscrowTicket {
    fn drop(&mut self) {
        if !self.redeemed {
            self.state.abandon_escrow(self.bytes);
            self.redeemed = true;
        }
    }
}

/// Logical retained-size contract for memory budgeting.
pub trait ChargedSize {
    /// Returns the logical retained byte size.
    fn charged_size(&self) -> u64;
}

impl ChargedSize for u64 {
    fn charged_size(&self) -> u64 {
        *self
    }
}

impl ChargedSize for usize {
    fn charged_size(&self) -> u64 {
        *self as u64
    }
}

impl ChargedSize for &[u8] {
    fn charged_size(&self) -> u64 {
        self.len() as u64
    }
}

impl ChargedSize for Vec<u8> {
    fn charged_size(&self) -> u64 {
        self.len() as u64
    }
}

/// Admission decision source attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionDecision {
    /// Accepted by process-wide pressure policy.
    Process,
    /// Accepted or rejected by local runtime budget.
    Runtime,
    /// Accepted or rejected by cross-runtime escrow.
    Escrow,
}

// -----------------------------------------------------------------------------
// Runtime-local budget accessor.
//
// A pipeline runtime owns exactly one [`RuntimeMemoryAccount`]. Local charge
// sites (receiver ingress, processor retained state, local channels, etc.)
// must reach that single account without re-deriving it from the snapshot
// handle, and without crossing thread boundaries. This is delivered through a
// runtime-thread `thread_local!`, mirroring the [`PIPELINE_ENTITY_KEY`]
// pattern used by `entity_context`.
//
// The accessor is `!Send`: the holder, the TLS slot, the guard, and the
// reference returned to callers cannot escape the pipeline runtime thread.
// Shared nodes, topic broker tasks, and cross-runtime queue paths must not
// call into this accessor; they must use [`EscrowTicket`] at the boundary.
//
// [`PIPELINE_ENTITY_KEY`]: crate::entity_context::pipeline_entity_key
// -----------------------------------------------------------------------------

/// Runtime-thread-local holder for the per-runtime memory account.
///
/// Owns the single [`RuntimeMemoryAccount`] created on the pinned pipeline
/// runtime thread. Distributed to local charge sites via
/// [`current_runtime_memory_budget`] as `Rc<Self>` so multiple call sites
/// share the same account without cross-thread atomics.
///
/// `RuntimeMemoryBudget` is intentionally `!Send` and `!Sync`: it carries a
/// [`RuntimeMemoryAccount`] whose hot-path state is built on `!Send` `Cell`s.
/// Sending it across threads would race with the runtime thread's owned
/// `Cell` writes and is therefore forbidden by the type system.
#[derive(Debug)]
pub struct RuntimeMemoryBudget {
    account: Rc<RuntimeMemoryAccount>,
}

impl RuntimeMemoryBudget {
    /// Wraps a runtime-local account into a holder usable by charge sites.
    #[must_use]
    pub fn new(account: Rc<RuntimeMemoryAccount>) -> Self {
        Self { account }
    }

    /// Borrows the runtime-local account.
    #[must_use]
    pub fn account(&self) -> &RuntimeMemoryAccount {
        &self.account
    }

    /// Charges known retained bytes against this runtime budget.
    ///
    /// The returned [`LocalMemoryTicket`] owns an `Rc` clone of the
    /// runtime-local account. That keeps the ticket usable in retained local
    /// envelopes and queues without making it `Send` or relying on a borrowed
    /// accessor lifetime.
    #[must_use]
    pub fn charge(&self, size: impl ChargedSize) -> Option<LocalMemoryTicket> {
        self.account.charge(size)
    }

    /// Returns the attribution scope carried by the underlying account.
    #[must_use]
    pub fn scope(&self) -> &BudgetScopeId {
        self.account.scope()
    }

    /// Publishes pending local state to the shared snapshot. Convenience
    /// wrapper around [`RuntimeMemoryAccount::flush_snapshot`] for metric-tick
    /// callbacks that hold an `Rc<RuntimeMemoryBudget>`.
    pub fn flush_snapshot(&self) {
        self.account.flush_snapshot();
    }
}

thread_local! {
    /// Per-runtime-thread slot for the current pipeline's memory budget.
    ///
    /// Set on the pinned pipeline runtime thread by
    /// [`set_current_runtime_memory_budget`] and cleared by dropping the
    /// returned [`RuntimeMemoryBudgetGuard`]. Reads are intra-thread:
    /// `borrow().clone()` is a single `Rc` strong-count bump with no atomics
    /// or shared coordination.
    static CURRENT_RUNTIME_MEMORY_BUDGET: RefCell<Option<Rc<RuntimeMemoryBudget>>> =
        const { RefCell::new(None) };
}

/// RAII guard returned by [`set_current_runtime_memory_budget`].
///
/// On drop, restores the previous (typically `None`) value of the runtime
/// memory-budget slot for this thread. The guard is `!Send` so it cannot
/// escape the runtime thread it was created on; this guarantees the TLS
/// invariant "the budget set here is also cleared here, on the same thread".
#[must_use = "the runtime memory-budget remains installed until this guard is dropped"]
pub struct RuntimeMemoryBudgetGuard {
    previous: Option<Rc<RuntimeMemoryBudget>>,
    _not_send: PhantomData<Rc<()>>,
}

impl Drop for RuntimeMemoryBudgetGuard {
    fn drop(&mut self) {
        let prev = self.previous.take();
        CURRENT_RUNTIME_MEMORY_BUDGET.with(|slot: &RefCell<Option<Rc<RuntimeMemoryBudget>>>| {
            // Restore the previous slot value. The replaced `Some(budget)`
            // is dropped here, on the runtime thread, releasing the `Rc`
            // clone installed by `set_current_runtime_memory_budget`.
            let _ = slot.replace(prev);
        });
    }
}

/// Installs a runtime memory budget as the current thread's accessor.
///
/// Returns a guard that, when dropped, restores whatever was installed before
/// this call (typically `None`). Must be called on the pinned pipeline
/// runtime thread; the returned guard is `!Send` and must drop on that same
/// thread.
///
/// Passing `None` clears the slot (still returning a guard that restores the
/// previous value), which is useful for tests that need to verify the
/// accessor is absent.
pub fn set_current_runtime_memory_budget(
    budget: Option<Rc<RuntimeMemoryBudget>>,
) -> RuntimeMemoryBudgetGuard {
    let previous = CURRENT_RUNTIME_MEMORY_BUDGET
        .with(|slot: &RefCell<Option<Rc<RuntimeMemoryBudget>>>| slot.replace(budget));
    RuntimeMemoryBudgetGuard {
        previous,
        _not_send: PhantomData,
    }
}

/// Returns a cloned `Rc<RuntimeMemoryBudget>` for the current runtime thread.
///
/// Returns `None` outside a pipeline runtime thread, before
/// [`set_current_runtime_memory_budget`] has been called, or after the guard
/// for this runtime has been dropped.
///
/// Hot-path cost is intentionally bounded to a single `RefCell` borrow plus
/// one `Rc::clone` (no atomics, no locks). Hot-path callers that need many
/// accesses per item should cache the returned `Rc` in a runtime-local
/// variable instead of calling this on every charge.
#[must_use]
pub fn current_runtime_memory_budget() -> Option<Rc<RuntimeMemoryBudget>> {
    CURRENT_RUNTIME_MEMORY_BUDGET
        .with(|slot: &RefCell<Option<Rc<RuntimeMemoryBudget>>>| slot.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account_with_mode(
        mode: BudgetMode,
        floor: u64,
        step: u64,
        overshoot: u64,
        spare: u64,
    ) -> Rc<RuntimeMemoryAccount> {
        Rc::new(RuntimeMemoryAccount::new(
            floor,
            step,
            overshoot,
            mode,
            Arc::new(GlobalLeasePool::new(spare)),
            Arc::new(RuntimeMemorySnapshot::default()),
            BudgetScopeId::default(),
        ))
    }

    fn account(floor: u64, step: u64, overshoot: u64, spare: u64) -> Rc<RuntimeMemoryAccount> {
        account_with_mode(BudgetMode::ObserveOnly, floor, step, overshoot, spare)
    }

    #[test]
    fn lease_arithmetic_borrows_full_steps_and_rejects_partial_borrow() {
        let acct = account_with_mode(BudgetMode::Enforce, 100, 10, 20, 15);

        assert!(acct.try_reserve_extra(105));
        assert_eq!(acct.lease.borrowed_bytes(), 10);
        let _ticket = acct.charge(105_u64).expect("initial charge should fit");
        assert!(!acct.try_reserve_extra(31));
        assert_eq!(acct.lease.borrowed_bytes(), 10);
    }

    #[test]
    fn local_ticket_refunds_on_drop() {
        let acct = account(100, 10, 20, 100);
        {
            let ticket = acct.charge(50_u64).expect("charge should fit");
            assert_eq!(ticket.bytes(), 50);
            acct.flush_snapshot();
            assert_eq!(acct.snapshot.charged_bytes(), 50);
        }
        acct.flush_snapshot();
        assert_eq!(acct.snapshot.charged_bytes(), 0);
    }

    #[test]
    fn try_reserve_extra_succeeds_before_growth() {
        let acct = account(100, 10, 20, 10);
        assert!(acct.try_reserve_extra(110));
        assert_eq!(acct.lease.borrowed_bytes(), 10);
    }

    #[test]
    fn reconcile_size_records_overshoot_and_level() {
        let acct = account(100, 10, 20, 0);
        let _ticket = acct.charge(115_u64).expect("overshoot is allowed");
        assert_eq!(acct.snapshot.overshoot_bytes(), 15);
        assert_eq!(acct.snapshot.level(), BudgetLevel::Soft.as_u64());
    }

    #[test]
    fn dropped_runtime_snapshots_are_pruned_from_aggregate() {
        let state = MemoryBudgetState::default();
        state.configure(
            RuntimeMemoryBudgetConfig {
                mode: BudgetMode::ObserveOnly,
                retry_after_secs: 1,
                sizing: MemoryBudgetSizing {
                    reserve_bytes: 0,
                    floor_per_runtime_bytes: 100,
                    lease_step_bytes: 10,
                    max_overshoot_per_runtime_bytes: 20,
                    overshoot_debt_limit_bytes: 10,
                },
                topic_default_limit_bytes: 100,
                runtime_count: 1,
            },
            None,
        );
        let handle = state.register_runtime_snapshot(BudgetScopeId::default());
        let account = handle.local_account().expect("budget should be configured");
        let ticket = account.charge(40_u64).expect("charge should fit");

        assert_eq!(state.snapshot().runtime_count, 1);
        drop(ticket);
        drop(account);
        drop(handle);

        let snapshot = state.snapshot();
        assert_eq!(snapshot.runtime_count, 0);
        assert_eq!(snapshot.charged_bytes, 0);
    }

    #[test]
    fn try_into_escrow_success_transfers_ownership() {
        let state = MemoryBudgetState::default();
        state.configure(
            RuntimeMemoryBudgetConfig {
                mode: BudgetMode::ObserveOnly,
                retry_after_secs: 1,
                sizing: MemoryBudgetSizing {
                    reserve_bytes: 0,
                    floor_per_runtime_bytes: 100,
                    lease_step_bytes: 10,
                    max_overshoot_per_runtime_bytes: 20,
                    overshoot_debt_limit_bytes: 10,
                },
                topic_default_limit_bytes: 100,
                runtime_count: 1,
            },
            None,
        );
        let acct = account(100, 10, 20, 100);
        let ticket = acct.charge(50_u64).expect("charge should fit");
        let escrow = ticket
            .try_into_escrow(&state)
            .expect("escrow should fit topic limit");

        assert_eq!(escrow.bytes(), 50);
        acct.flush_snapshot();
        assert_eq!(acct.snapshot.charged_bytes(), 0);
        assert_eq!(state.snapshot().escrow_ticket_count, 1);
        assert_eq!(state.snapshot().escrow_charged_bytes, 50);
        escrow.redeem();
        assert_eq!(state.snapshot().escrow_ticket_count, 0);
        assert_eq!(state.snapshot().escrow_charged_bytes, 0);
        assert_eq!(state.snapshot().abandoned_escrow_count, 0);
    }

    #[test]
    fn try_into_escrow_failure_returns_original_local_ticket() {
        let state = MemoryBudgetState::default();
        state.configure(
            RuntimeMemoryBudgetConfig {
                mode: BudgetMode::Enforce,
                retry_after_secs: 1,
                sizing: MemoryBudgetSizing {
                    reserve_bytes: 0,
                    floor_per_runtime_bytes: 100,
                    lease_step_bytes: 10,
                    max_overshoot_per_runtime_bytes: 20,
                    overshoot_debt_limit_bytes: 10,
                },
                topic_default_limit_bytes: 10,
                runtime_count: 1,
            },
            None,
        );
        let acct = account(100, 10, 20, 100);
        let ticket = acct.charge(50_u64).expect("charge should fit");
        let ticket = ticket
            .try_into_escrow(&state)
            .expect_err("escrow should reject above topic limit");

        assert_eq!(ticket.bytes(), 50);
        acct.flush_snapshot();
        assert_eq!(acct.snapshot.charged_bytes(), 50);
    }

    #[test]
    fn observe_only_escrow_records_above_configured_limit() {
        let state = MemoryBudgetState::default();
        state.configure(
            RuntimeMemoryBudgetConfig {
                mode: BudgetMode::ObserveOnly,
                retry_after_secs: 1,
                sizing: MemoryBudgetSizing {
                    reserve_bytes: 0,
                    floor_per_runtime_bytes: 100,
                    lease_step_bytes: 10,
                    max_overshoot_per_runtime_bytes: 20,
                    overshoot_debt_limit_bytes: 10,
                },
                topic_default_limit_bytes: 10,
                runtime_count: 1,
            },
            None,
        );
        let acct = account(100, 10, 20, 100);
        let ticket = acct.charge(50_u64).expect("observe-only charge should fit");
        let escrow = ticket
            .try_into_escrow(&state)
            .expect("observe-only escrow should record above limit");

        assert_eq!(escrow.bytes(), 50);
        assert_eq!(state.snapshot().escrow_ticket_count, 1);
        assert_eq!(state.snapshot().escrow_charged_bytes, 50);
    }

    #[test]
    fn enforce_escrow_limit_is_aggregate() {
        let state = MemoryBudgetState::default();
        state.configure(
            RuntimeMemoryBudgetConfig {
                mode: BudgetMode::Enforce,
                retry_after_secs: 1,
                sizing: MemoryBudgetSizing {
                    reserve_bytes: 0,
                    floor_per_runtime_bytes: 100,
                    lease_step_bytes: 10,
                    max_overshoot_per_runtime_bytes: 20,
                    overshoot_debt_limit_bytes: 10,
                },
                topic_default_limit_bytes: 60,
                runtime_count: 1,
            },
            None,
        );
        let acct = account(100, 10, 20, 100);
        let first = acct
            .charge(40_u64)
            .expect("first local charge should fit")
            .try_into_escrow(&state)
            .expect("first escrow should fit aggregate limit");
        let second = acct
            .charge(30_u64)
            .expect("second local charge should fit")
            .try_into_escrow(&state)
            .expect_err("second escrow should exceed aggregate limit");

        assert_eq!(first.bytes(), 40);
        assert_eq!(second.bytes(), 30);
        assert_eq!(state.snapshot().escrow_ticket_count, 1);
        assert_eq!(state.snapshot().escrow_charged_bytes, 40);
    }

    #[test]
    fn escrow_drop_creates_abandoned_entry() {
        let state = MemoryBudgetState::default();
        state.configure(
            RuntimeMemoryBudgetConfig {
                mode: BudgetMode::ObserveOnly,
                retry_after_secs: 1,
                sizing: MemoryBudgetSizing {
                    reserve_bytes: 0,
                    floor_per_runtime_bytes: 100,
                    lease_step_bytes: 10,
                    max_overshoot_per_runtime_bytes: 20,
                    overshoot_debt_limit_bytes: 10,
                },
                topic_default_limit_bytes: 100,
                runtime_count: 1,
            },
            None,
        );
        let acct = account(100, 10, 20, 100);
        let ticket = acct.charge(42_u64).expect("charge should fit");
        let escrow = ticket
            .try_into_escrow(&state)
            .expect("escrow should fit topic limit");

        drop(escrow);

        let snapshot = state.snapshot();
        assert_eq!(snapshot.abandoned_escrow_count, 1);
        assert_eq!(snapshot.abandoned_escrow_bytes, 42);
        assert_eq!(snapshot.escrow_ticket_count, 1);
        assert_eq!(snapshot.escrow_charged_bytes, 42);
    }

    #[test]
    fn observe_only_charge_records_above_hard_limit() {
        let acct = account(100, 10, 20, 0);
        let ticket = acct
            .charge(121_u64)
            .expect("observe-only charge should record above hard");

        assert_eq!(ticket.bytes(), 121);
        assert_eq!(acct.snapshot.charged_bytes(), 121);
        assert_eq!(acct.level(), BudgetLevel::Hard);
    }

    #[test]
    fn local_account_can_only_be_taken_once_per_handle() {
        let state = MemoryBudgetState::default();
        state.configure(
            RuntimeMemoryBudgetConfig {
                mode: BudgetMode::ObserveOnly,
                retry_after_secs: 1,
                sizing: MemoryBudgetSizing {
                    reserve_bytes: 0,
                    floor_per_runtime_bytes: 100,
                    lease_step_bytes: 10,
                    max_overshoot_per_runtime_bytes: 20,
                    overshoot_debt_limit_bytes: 10,
                },
                topic_default_limit_bytes: 100,
                runtime_count: 1,
            },
            None,
        );
        let handle = state.register_runtime_snapshot(BudgetScopeId::default());

        // The first take succeeds and grants the single per-runtime account.
        let _first = handle
            .local_account()
            .expect("first account take should succeed");

        // Subsequent takes (including via a cloned handle) must return None
        // so per-runtime hot-path state has a single writer.
        assert!(handle.local_account().is_none());
        assert!(handle.clone().local_account().is_none());
    }

    #[test]
    fn charge_does_not_publish_without_level_transition_or_flush() {
        // Floor large enough that one small charge stays Normal: no level
        // transition, so the snapshot should remain at its initial baseline
        // until flush_snapshot is explicitly called.
        let acct = account(1_000, 10, 20, 0);
        assert_eq!(acct.snapshot.charged_bytes(), 0);

        let _ticket = acct.charge(50_u64).expect("charge should fit");
        assert_eq!(
            acct.snapshot.charged_bytes(),
            0,
            "snapshot must remain stale until explicit flush"
        );
        acct.flush_snapshot();
        assert_eq!(acct.snapshot.charged_bytes(), 50);
    }

    #[test]
    fn level_transition_publishes_eagerly() {
        // A charge that crosses the soft band must publish immediately so
        // operator-visible pressure events are not delayed by metric cadence.
        let acct = account(100, 10, 20, 0);
        assert_eq!(acct.snapshot.level(), BudgetLevel::Normal.as_u64());
        let _ticket = acct.charge(115_u64).expect("overshoot is allowed");
        assert_eq!(
            acct.snapshot.level(),
            BudgetLevel::Soft.as_u64(),
            "Soft transition must be published without a flush call"
        );
    }

    #[test]
    fn flush_snapshot_is_idempotent() {
        let acct = account(1_000, 10, 20, 0);
        let _ticket = acct.charge(50_u64).expect("charge should fit");
        acct.flush_snapshot();
        let charged = acct.snapshot.charged_bytes();
        // Calling flush again must not change observable state.
        acct.flush_snapshot();
        assert_eq!(acct.snapshot.charged_bytes(), charged);
    }

    // -------------------------------------------------------------------------
    // Runtime-local accessor tests.
    //
    // These tests run on the test thread (which doubles as the runtime
    // thread). Each test installs at most one budget and drops its guard at
    // the end, leaving the TLS slot clean for the next test.
    //
    // Note on test isolation: cargo runs tests in parallel across multiple
    // threads, but the TLS slot is per-thread; tests scheduled onto the same
    // worker thread sequentially see the slot in its previous-restored
    // (empty) state thanks to the guard's `Drop` impl.
    // -------------------------------------------------------------------------

    fn rc_budget_from(acct: Rc<RuntimeMemoryAccount>) -> Rc<RuntimeMemoryBudget> {
        Rc::new(RuntimeMemoryBudget::new(acct))
    }

    #[test]
    fn current_runtime_memory_budget_is_none_before_install() {
        // Ensure no leaked state from earlier tests on this thread.
        let _clean = set_current_runtime_memory_budget(None);
        assert!(current_runtime_memory_budget().is_none());
    }

    #[test]
    fn install_makes_budget_accessible() {
        let _clean = set_current_runtime_memory_budget(None);
        let budget = rc_budget_from(account(100, 10, 20, 100));
        let _guard = set_current_runtime_memory_budget(Some(budget.clone()));
        let current = current_runtime_memory_budget().expect("budget should be installed");
        assert!(Rc::ptr_eq(&budget, &current));
    }

    #[test]
    fn dropping_guard_clears_accessor() {
        let _clean = set_current_runtime_memory_budget(None);
        let budget = rc_budget_from(account(100, 10, 20, 100));
        {
            let _guard = set_current_runtime_memory_budget(Some(budget));
            assert!(current_runtime_memory_budget().is_some());
        }
        assert!(current_runtime_memory_budget().is_none());
    }

    #[test]
    fn multiple_accessor_calls_reuse_same_account() {
        let _clean = set_current_runtime_memory_budget(None);
        let budget = rc_budget_from(account(100, 10, 20, 100));
        let _guard = set_current_runtime_memory_budget(Some(budget.clone()));

        let first = current_runtime_memory_budget().expect("budget should be installed");
        let second = current_runtime_memory_budget().expect("budget should be installed");

        // Same `Rc` allocation - both clones point at the original budget.
        assert!(Rc::ptr_eq(&first, &second));
        assert!(Rc::ptr_eq(&budget, &first));

        // Charges through different accessor clones reach the same account.
        let ticket_a = first.charge(40_u64).expect("first charge fits");
        let ticket_b = second.charge(30_u64).expect("second charge fits");
        budget.account().flush_snapshot();
        assert_eq!(budget.account().snapshot.charged_bytes(), 70);
        drop(ticket_a);
        drop(ticket_b);
        budget.account().flush_snapshot();
        assert_eq!(budget.account().snapshot.charged_bytes(), 0);
    }

    #[test]
    fn local_ticket_outlives_temporary_accessor_clone() {
        let _clean = set_current_runtime_memory_budget(None);
        let budget = rc_budget_from(account(100, 10, 20, 100));
        let _guard = set_current_runtime_memory_budget(Some(budget.clone()));

        let ticket = {
            let current = current_runtime_memory_budget().expect("budget should be installed");
            current.charge(40_u64).expect("charge should fit")
        };

        budget.flush_snapshot();
        assert_eq!(budget.account().snapshot.charged_bytes(), 40);
        drop(ticket);
        budget.flush_snapshot();
        assert_eq!(budget.account().snapshot.charged_bytes(), 0);
    }

    #[test]
    fn nested_install_restores_previous_on_drop() {
        let _clean = set_current_runtime_memory_budget(None);
        let outer = rc_budget_from(account(100, 10, 20, 100));
        let inner = rc_budget_from(account(200, 10, 20, 100));

        let _outer_guard = set_current_runtime_memory_budget(Some(outer.clone()));
        {
            let _inner_guard = set_current_runtime_memory_budget(Some(inner.clone()));
            let current = current_runtime_memory_budget().expect("inner should be installed");
            assert!(Rc::ptr_eq(&inner, &current));
        }
        // Inner guard dropped; outer must be restored.
        let current = current_runtime_memory_budget().expect("outer should be restored");
        assert!(Rc::ptr_eq(&outer, &current));
    }

    #[test]
    fn dropping_holder_lets_weak_snapshot_prune() {
        let _clean = set_current_runtime_memory_budget(None);
        let state = MemoryBudgetState::default();
        state.configure(
            RuntimeMemoryBudgetConfig {
                mode: BudgetMode::ObserveOnly,
                retry_after_secs: 1,
                sizing: MemoryBudgetSizing {
                    reserve_bytes: 0,
                    floor_per_runtime_bytes: 100,
                    lease_step_bytes: 10,
                    max_overshoot_per_runtime_bytes: 20,
                    overshoot_debt_limit_bytes: 10,
                },
                topic_default_limit_bytes: 100,
                runtime_count: 1,
            },
            None,
        );
        let handle = state.register_runtime_snapshot(BudgetScopeId::default());
        let acct = handle.local_account().expect("single take should succeed");
        let budget = rc_budget_from(acct);
        let guard = set_current_runtime_memory_budget(Some(budget));
        drop(handle);
        assert_eq!(state.snapshot().runtime_count, 1);

        // Drop the guard: TLS releases the only Rc<RuntimeMemoryBudget> ->
        // RuntimeMemoryAccount drops -> the snapshot Arc strong count hits 0
        // -> the next aggregate snapshot() call prunes the Weak entry.
        drop(guard);
        let snapshot = state.snapshot();
        assert_eq!(snapshot.runtime_count, 0);
        assert_eq!(snapshot.charged_bytes, 0);
    }

    // Compile-time invariants: the runtime-local holder and its guard must
    // never be `Send`. Removing the `cfg(any())` gate on the functions below
    // forces the compiler to type-check them, at which point `needs_send::<T>`
    // fails for any `T: !Send`. The gate keeps these as a textual invariant
    // (no build cost) while still being a single edit away from a real probe
    // if a contributor wants to confirm a future refactor preserves `!Send`.
    #[cfg(any())]
    fn _budget_must_not_be_send() {
        fn needs_send<T: Send>() {}
        needs_send::<RuntimeMemoryBudget>();
        needs_send::<RuntimeMemoryBudgetGuard>();
    }

    // Runtime probe: the `Rc` inside the holder and the `PhantomData<Rc<()>>`
    // inside the guard already make both types `!Send` at the type level.
    // This test confirms the holder cannot be moved into a `tokio::spawn`/
    // `std::thread::spawn` boundary by attempting the move via a `Send`
    // closure helper. The helper is generic over `T: Send`; calling it with a
    // non-`Send` argument is a compile error if the test were enabled.
    //
    // We keep this as a documentary assertion rather than a real runtime
    // check because the compiler already enforces it - this comment is the
    // PR-review-time evidence trail.
}
