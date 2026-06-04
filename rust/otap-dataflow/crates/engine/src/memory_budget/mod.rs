// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Observe-only runtime memory budgeting primitives.
//!
//! This module intentionally does not enforce admission. It defines the local
//! and cross-runtime ownership types used by later milestones and exposes shared
//! snapshots for engine-level metrics.

use otap_df_config::policy::{MemoryBudgetMode as ConfigBudgetMode, MemoryBudgetPolicy};
use std::cell::Cell;
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Immutable attribution carried by budget owners when the identity is known.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BudgetScopeId {
    /// Pipeline group owning or attributing the retained bytes.
    pub pipeline_group_id: Option<String>,
    /// Pipeline owning or attributing the retained bytes.
    pub pipeline_id: Option<String>,
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
}

#[derive(Debug, Default)]
struct MemoryBudgetStateInner {
    config: Mutex<Option<RuntimeMemoryBudgetConfig>>,
    pool: GlobalLeasePool,
    snapshots: Mutex<Vec<Arc<RuntimeMemorySnapshot>>>,
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
    #[must_use]
    pub fn register_runtime_snapshot(&self) -> RuntimeMemorySnapshotHandle {
        let snapshot = Arc::new(RuntimeMemorySnapshot::default());
        if let Some(config) = self.config() {
            snapshot.publish(
                0,
                0,
                0,
                0,
                if config.mode == BudgetMode::ObserveOnly {
                    BudgetLevel::Normal
                } else {
                    BudgetLevel::Hard
                },
            );
        }
        self.inner
            .snapshots
            .lock()
            .expect("memory budget snapshots poisoned")
            .push(snapshot.clone());
        RuntimeMemorySnapshotHandle {
            snapshot,
            state: self.clone(),
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
        let snapshots = self
            .inner
            .snapshots
            .lock()
            .expect("memory budget snapshots poisoned");
        let mut snapshot = MemoryBudgetSnapshot {
            runtime_count: snapshots.len() as u64,
            abandoned_escrow_count: self.inner.abandoned_escrow_count.load(Ordering::Relaxed),
            abandoned_escrow_bytes: self.inner.abandoned_escrow_bytes.load(Ordering::Relaxed),
            escrow_ticket_count: self.inner.escrow_ticket_count.load(Ordering::Relaxed),
            escrow_charged_bytes: self.inner.escrow_charged_bytes.load(Ordering::Relaxed),
            ..MemoryBudgetSnapshot::default()
        };
        for runtime in snapshots.iter() {
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

    fn escrow_accepts(&self, bytes: u64) -> bool {
        bytes
            <= self
                .config()
                .map_or(0, |config| config.topic_default_limit_bytes)
    }

    fn charge_escrow(&self, bytes: u64) {
        let _ = self
            .inner
            .escrow_ticket_count
            .fetch_add(1, Ordering::Relaxed);
        let _ = self
            .inner
            .escrow_charged_bytes
            .fetch_add(bytes, Ordering::Relaxed);
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
#[derive(Debug, Clone)]
pub struct RuntimeMemorySnapshotHandle {
    snapshot: Arc<RuntimeMemorySnapshot>,
    state: MemoryBudgetState,
}

impl RuntimeMemorySnapshotHandle {
    /// Creates a local runtime account from this snapshot handle.
    #[must_use]
    pub fn local_account(&self) -> Option<RuntimeMemoryAccount> {
        let config = self.state.config()?;
        Some(RuntimeMemoryAccount::new(
            config.sizing.floor_per_runtime_bytes,
            config.sizing.lease_step_bytes,
            config.sizing.max_overshoot_per_runtime_bytes,
            self.state.lease_authority(),
            self.snapshot.clone(),
            BudgetScopeId::default(),
        ))
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
#[derive(Debug)]
pub struct RuntimeMemoryAccount {
    scope: BudgetScopeId,
    floor_bytes: u64,
    lease: LocalMemoryLease,
    charged_bytes: Cell<u64>,
    unknown_bytes: Cell<u64>,
    overshoot_bytes: Cell<u64>,
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
        lease_authority: Arc<dyn LeaseAuthority>,
        snapshot: Arc<RuntimeMemorySnapshot>,
        scope: BudgetScopeId,
    ) -> Self {
        let account = Self {
            scope,
            floor_bytes,
            lease: LocalMemoryLease::new(lease_step_bytes, max_overshoot_bytes, lease_authority),
            charged_bytes: Cell::new(0),
            unknown_bytes: Cell::new(0),
            overshoot_bytes: Cell::new(0),
            snapshot,
            _not_send: PhantomData,
        };
        account.publish();
        account
    }

    /// Returns the current pressure level.
    #[must_use]
    pub fn level(&self) -> BudgetLevel {
        self.classify(self.charged_bytes.get())
    }

    /// Attempts to reserve additional bytes before growth.
    #[must_use]
    pub fn try_reserve_extra(&self, bytes: u64) -> bool {
        let next = self.charged_bytes.get().saturating_add(bytes);
        if next <= self.available_without_overshoot() {
            return true;
        }
        let needed = next.saturating_sub(self.available_without_overshoot());
        if self.lease.try_borrow_for(needed) {
            self.publish();
            return true;
        }
        next <= self.hard_limit()
    }

    /// Charges known retained bytes and returns a local ticket.
    #[must_use]
    pub fn charge<'a>(&'a self, size: impl ChargedSize) -> Option<LocalMemoryTicket<'a>> {
        let bytes = size.charged_size();
        if !self.try_reserve_extra(bytes) {
            return None;
        }
        self.charged_bytes
            .set(self.charged_bytes.get().saturating_add(bytes));
        self.reconcile_size();
        Some(LocalMemoryTicket {
            account: self,
            bytes,
            active: true,
            scope: self.scope.clone(),
            _not_send: PhantomData,
        })
    }

    /// Records retained bytes whose logical size is unknown.
    pub fn observe_unknown(&self, bytes: u64) {
        self.unknown_bytes
            .set(self.unknown_bytes.get().saturating_add(bytes));
        self.publish();
    }

    /// Reconciles current charged size and updates pressure/overshoot.
    pub fn reconcile_size(&self) {
        let charged = self.charged_bytes.get();
        let overshoot = charged.saturating_sub(self.available_without_overshoot());
        self.overshoot_bytes.set(overshoot);
        self.lease.return_lazy(charged, self.floor_bytes);
        self.publish();
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

    fn publish(&self) {
        self.snapshot.publish(
            self.lease.borrowed_bytes(),
            self.charged_bytes.get(),
            self.unknown_bytes.get(),
            self.overshoot_bytes.get(),
            self.level(),
        );
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
            return_watermark_bytes: lease_step_bytes.saturating_mul(2),
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
pub struct LocalMemoryTicket<'a> {
    account: &'a RuntimeMemoryAccount,
    bytes: u64,
    active: bool,
    scope: BudgetScopeId,
    _not_send: PhantomData<Rc<()>>,
}

impl<'a> LocalMemoryTicket<'a> {
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
    ) -> Result<EscrowTicket, LocalMemoryTicket<'a>> {
        let bytes = self.bytes;
        if !state.escrow_accepts(bytes) {
            return Err(self);
        }
        self.active = false;
        self.account.refund(bytes);
        state.charge_escrow(bytes);
        Ok(EscrowTicket {
            bytes,
            state: state.clone(),
            scope: self.scope.clone(),
            redeemed: false,
        })
    }
}

impl Drop for LocalMemoryTicket<'_> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn account(floor: u64, step: u64, overshoot: u64, spare: u64) -> RuntimeMemoryAccount {
        RuntimeMemoryAccount::new(
            floor,
            step,
            overshoot,
            Arc::new(GlobalLeasePool::new(spare)),
            Arc::new(RuntimeMemorySnapshot::default()),
            BudgetScopeId::default(),
        )
    }

    #[test]
    fn lease_arithmetic_borrows_full_steps_and_rejects_partial_borrow() {
        let acct = account(100, 10, 20, 15);

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
            assert_eq!(acct.snapshot.charged_bytes(), 50);
        }
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
        let ticket = acct.charge(50_u64).expect("charge should fit");
        let ticket = ticket
            .try_into_escrow(&state)
            .expect_err("escrow should reject above topic limit");

        assert_eq!(ticket.bytes(), 50);
        assert_eq!(acct.snapshot.charged_bytes(), 50);
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
    fn normal_reservation_rejects_unbounded_first_item() {
        let acct = account(100, 10, 20, 0);
        assert!(acct.try_reserve_extra(120));
        assert_eq!(acct.level(), BudgetLevel::Normal);
        assert!(!acct.try_reserve_extra(121));
    }
}
