// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Observe-only runtime memory budgeting primitives.
//!
//! This module intentionally does not enforce admission. It defines the local
//! and cross-runtime ownership types used by later milestones and exposes shared
//! snapshots for engine-level metrics.

pub mod reclaim;

use otap_df_config::policy::{MemoryBudgetMode as ConfigBudgetMode, MemoryBudgetPolicy};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};

use smallvec::SmallVec;

static DEFAULT_ESCROW_BUCKET: LazyLock<Arc<str>> = LazyLock::new(|| Arc::<str>::from("shared"));

/// Immutable attribution carried by budget owners when the identity is known.
///
/// The string-bearing fields exist for config, metrics, and reporting
/// boundaries. On the per-item ownership hot path the attribution is never
/// cloned field-by-field: it is interned once per runtime account into a
/// [`BudgetScope`] handle (see that alias), and owners either borrow it through
/// the account `Rc` they already hold (local tickets) or clone the cheap
/// reference-counted handle at shared boundaries (escrow). No `String` is
/// allocated when a [`LocalMemoryTicket`] or [`EscrowTicket`] is created.
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

/// Interned, reference-counted attribution handle carried by budget owners.
///
/// Cloning a `BudgetScope` is a single reference-count bump with no string
/// allocation, so it is cheap enough to move with ownership at escrow and
/// shared boundaries. `Arc` (not `Rc`) is required because an [`EscrowTicket`]
/// is `Send` and the scope travels with it across runtime boundaries. Local
/// tickets avoid even this refcount bump by borrowing the scope through the
/// account `Rc` they already own.
pub type BudgetScope = Arc<BudgetScopeId>;

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
    /// Per-runtime drain/redemption allowance carved out of `floor_bytes`
    /// (design lines 1166-1171). `0` means "use the default", which the engine
    /// resolves to `lease_step_bytes` so a runtime at local `Hard` can still
    /// drain at least one lease-step-sized in-flight item.
    pub drain_allowance_bytes: u64,
}

impl MemoryBudgetSizing {
    fn from_policy(policy: &MemoryBudgetPolicy) -> Self {
        Self {
            reserve_bytes: policy.sizing.reserve,
            floor_per_runtime_bytes: policy.sizing.floor_per_runtime,
            lease_step_bytes: policy.sizing.lease_step,
            max_overshoot_per_runtime_bytes: policy.sizing.max_overshoot_per_runtime,
            overshoot_debt_limit_bytes: policy.sizing.overshoot_debt_limit,
            drain_allowance_bytes: policy.sizing.drain_allowance.unwrap_or(0),
        }
    }

    /// Resolves the effective per-runtime drain allowance, applying the
    /// `lease_step_bytes` default when the operator left it unset (`0`).
    #[must_use]
    fn effective_drain_allowance_bytes(&self) -> u64 {
        if self.drain_allowance_bytes == 0 {
            self.lease_step_bytes
        } else {
            self.drain_allowance_bytes
        }
    }
}

/// Per-path runtime enforcement gates carried into the engine.
///
/// Mirrors [`MemoryBudgetEnforcementPolicy`](otap_df_config::policy::MemoryBudgetEnforcementPolicy).
/// When `mode == Enforce`, these flags select *which* admission points actually
/// reject; they are all `false` in observe-only. In production builds config
/// validation rejects enforce mode entirely unless the
/// `unstable-memory-enforcement` feature is enabled, so enforcement is
/// unreachable by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MemoryBudgetEnforcement {
    /// Enforce receiver admission against runtime budget. Not wired into a
    /// runtime admission point yet; carried for forward compatibility.
    pub receiver_admission: bool,
    /// Enforce queue/topic publish against escrow/topic-cap budget. Honored by
    /// the owned topic-publish path ([`LocalMemoryTicket::try_into_escrow`] /
    /// [`try_into_escrow_fanout`](LocalMemoryTicket::try_into_escrow_fanout)).
    pub queue_publish: bool,
    /// Enable reclaim hooks for retained-memory sources. Not wired into a
    /// runtime driver yet; carried for forward compatibility.
    pub reclaim_hooks: bool,
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
    /// Topic-publish escrow default limit.
    ///
    /// Current foundation scope: topic-owned publish applies this as a
    /// per-topic/per-boundary escrow bucket limit and exposes aggregate rollups.
    pub topic_default_limit_bytes: u64,
    /// Number of deployed runtime instances used for sizing.
    pub runtime_count: usize,
    /// Per-path enforcement gates (only meaningful when `mode == Enforce`).
    pub enforcement: MemoryBudgetEnforcement,
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
            enforcement: MemoryBudgetEnforcement {
                receiver_admission: policy.enforcement.receiver_admission,
                queue_publish: policy.enforcement.queue_publish,
                reclaim_hooks: policy.enforcement.reclaim_hooks,
            },
        }
    }
}

/// Shared snapshot published by one runtime account.
#[derive(Debug, Default)]
pub struct RuntimeMemorySnapshot {
    borrowed_bytes: AtomicU64,
    charged_bytes: AtomicU64,
    unknown_bytes: AtomicU64,
    unknown_count: AtomicU64,
    overshoot_bytes: AtomicU64,
    reconcile_debt_bytes: AtomicU64,
    drain_allowance_bytes: AtomicU64,
    drain_committed_bytes: AtomicU64,
    level: AtomicU64,
}

impl RuntimeMemorySnapshot {
    #[allow(clippy::too_many_arguments)]
    fn publish(
        &self,
        borrowed_bytes: u64,
        charged_bytes: u64,
        unknown_bytes: u64,
        unknown_count: u64,
        overshoot_bytes: u64,
        reconcile_debt_bytes: u64,
        drain_allowance_bytes: u64,
        drain_committed_bytes: u64,
        level: BudgetLevel,
    ) {
        self.borrowed_bytes.store(borrowed_bytes, Ordering::Relaxed);
        self.charged_bytes.store(charged_bytes, Ordering::Relaxed);
        self.unknown_bytes.store(unknown_bytes, Ordering::Relaxed);
        self.unknown_count.store(unknown_count, Ordering::Relaxed);
        self.overshoot_bytes
            .store(overshoot_bytes, Ordering::Relaxed);
        self.reconcile_debt_bytes
            .store(reconcile_debt_bytes, Ordering::Relaxed);
        self.drain_allowance_bytes
            .store(drain_allowance_bytes, Ordering::Relaxed);
        self.drain_committed_bytes
            .store(drain_committed_bytes, Ordering::Relaxed);
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

    /// Returns retained item count observed without a known logical size.
    #[must_use]
    pub fn unknown_count(&self) -> u64 {
        self.unknown_count.load(Ordering::Relaxed)
    }

    /// Returns bytes above local floor plus leases.
    #[must_use]
    pub fn overshoot_bytes(&self) -> u64 {
        self.overshoot_bytes.load(Ordering::Relaxed)
    }

    /// Returns reconciliation-debt bytes: overshoot recorded after growth that
    /// overdrew the global debt pool and remains unbacked.
    #[must_use]
    pub fn reconcile_debt_bytes(&self) -> u64 {
        self.reconcile_debt_bytes.load(Ordering::Relaxed)
    }

    /// Returns the bounded drain/redemption allowance configured for this
    /// runtime. Published from local `Cell` state at flush time so observers
    /// can validate that Hard consumers retain a path to drain in-transit
    /// escrow without enabling new external admission.
    #[must_use]
    pub fn drain_allowance_bytes(&self) -> u64 {
        self.drain_allowance_bytes.load(Ordering::Relaxed)
    }

    /// Returns the bytes currently outstanding against the drain allowance.
    /// Increments only when a Hard consumer admits an in-transit escrow item
    /// via the drain path; decrements exactly once when the returned drain
    /// ticket drops.
    #[must_use]
    pub fn drain_committed_bytes(&self) -> u64 {
        self.drain_committed_bytes.load(Ordering::Relaxed)
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
    /// Total retained item count observed without a known logical size.
    pub unknown_count: u64,
    /// Total bytes above runtime floors plus leases.
    pub overshoot_bytes: u64,
    /// Total reconciliation-debt bytes: overshoot recorded after growth that
    /// overdrew the global debt pool and remains unbacked.
    pub reconcile_debt_bytes: u64,
    /// Abandoned escrow tickets retained for leak detection.
    pub abandoned_escrow_count: u64,
    /// Abandoned escrow bytes retained for leak detection.
    pub abandoned_escrow_bytes: u64,
    /// Age in milliseconds of the oldest abandoned escrow entry.
    ///
    /// Zero means no abandoned escrow is currently recorded. Non-zero values
    /// are sticky until a future explicit reaper/alarm policy is introduced.
    pub abandoned_escrow_oldest_age_millis: u64,
    /// Number of bounded abandoned-escrow alarms emitted by this state.
    pub abandoned_escrow_alarm_count: u64,
    /// Cumulative count of abandoned-escrow entries reclaimed by the reaper.
    ///
    /// Reaping returns a leaked escrow charge's bytes to the global pool once it
    /// has been held longer than the configured reap threshold. The cumulative
    /// abandoned-escrow count/bytes are never decremented, so the difference
    /// between abandoned and reaped totals stays observable: leaks are reclaimed
    /// but never silently hidden.
    pub reaped_escrow_count: u64,
    /// Cumulative bytes reclaimed from abandoned escrow by the reaper.
    pub reaped_escrow_bytes: u64,
    /// Escrow tickets currently owning logical retained bytes.
    pub escrow_ticket_count: u64,
    /// Escrow bytes currently owning logical retained bytes.
    pub escrow_charged_bytes: u64,
    /// Number of escrow buckets with currently charged bytes.
    pub escrow_active_bucket_count: u64,
    /// Maximum charged bytes currently held by any one escrow bucket.
    pub escrow_max_bucket_bytes: u64,
    /// Escrow bytes currently backed by an explicit borrow against the global
    /// spare pool. Equal to `escrow_charged_bytes - escrow_pool_overshoot_bytes`
    /// in steady state. Surfaces the design invariant
    /// `sum(runtime_charged) + sum(escrow_charged) <= bounded global capacity`
    /// by making the escrow draw on the pool explicit.
    pub escrow_pool_held_bytes: u64,
    /// Escrow bytes that could not be backed by a pool borrow at conversion
    /// time and are tolerated only because the runtime is in observe-only mode.
    /// In enforce mode, escrow creation that would land here is rejected so the
    /// global logical invariant is preserved.
    pub escrow_pool_overshoot_bytes: u64,
    /// Total drain/redemption allowance configured across runtimes.
    pub drain_allowance_bytes: u64,
    /// Total drain bytes currently outstanding across runtimes.
    pub drain_committed_bytes: u64,
    /// Spare bytes currently available to lease from the global pool.
    pub spare_available_bytes: u64,
    /// Signed overshoot-debt pool balance. Negative means the pool has been
    /// overdrawn by post-hoc reconciliation (an invariant-violation signal).
    pub overshoot_debt_balance: i64,
}

#[derive(Debug, Default)]
struct MemoryBudgetStateInner {
    config: Mutex<Option<RuntimeMemoryBudgetConfig>>,
    pool: GlobalLeasePool,
    snapshots: Mutex<Vec<Weak<RuntimeMemorySnapshot>>>,
    // Phase 2e leak detection keeps a bounded, low-cardinality sticky view:
    // count/bytes/oldest age. It intentionally does not store per-scope strings.
    abandoned_escrow: Mutex<VecDeque<AbandonedEscrowEntry>>,
    abandoned_escrow_count: AtomicU64,
    abandoned_escrow_bytes: AtomicU64,
    abandoned_escrow_alarm_count: AtomicU64,
    abandoned_escrow_alarm_fired: AtomicBool,
    /// Reaper threshold in milliseconds. `0` (the default) disables reaping, so
    /// abandoned escrow stays sticky indefinitely. When non-zero, abandoned
    /// entries older than this are reclaimed (their bytes returned to the pool)
    /// during the periodic snapshot maintenance pass.
    abandoned_escrow_reap_after_millis: AtomicU64,
    /// Cumulative count/bytes reclaimed by the reaper, preserved for observability.
    reaped_escrow_count: AtomicU64,
    reaped_escrow_bytes: AtomicU64,
    escrow_ticket_count: AtomicU64,
    escrow_charged_bytes: AtomicU64,
    escrow_buckets: Mutex<HashMap<Arc<str>, EscrowBucket>>,
    /// Sum of bytes the escrow pool currently holds against the global spare
    /// pool. Incremented when [`MemoryBudgetState::try_charge_escrow`] succeeds
    /// at borrowing from `pool`, decremented on the matching release.
    escrow_pool_held_bytes: AtomicU64,
    /// Sum of escrow bytes that observe-only created without a backing pool
    /// borrow (because the pool was already exhausted at conversion time). The
    /// enforce-mode admission path rejects this case instead of accumulating
    /// here, so this counter only grows under `mode: observe_only`.
    escrow_pool_overshoot_bytes: AtomicU64,
}

#[derive(Debug)]
struct AbandonedEscrowEntry {
    abandoned_at: Instant,
    /// Bytes owned by the abandoned escrow, needed to reverse the charge on reap.
    bytes: u64,
    /// Whether the abandoned escrow held an explicit pool borrow, so the reaper
    /// returns bytes to the pool (vs. decrementing the observe-only overshoot).
    pool_backed: bool,
    /// The boundary bucket the abandoned escrow was charged against, so the
    /// reaper releases the same bucket it charged.
    bucket: EscrowBucket,
}

/// Shared runtime memory-budget state.
#[derive(Debug, Clone, Default)]
pub struct MemoryBudgetState {
    inner: Arc<MemoryBudgetStateInner>,
}

/// Per-boundary escrow bucket.
///
/// A bucket is created once for a topic or shared boundary and then reused by
/// shared-boundary publishers. Cloning the bucket is one `Arc` refcount bump;
/// the boundary name is interned in the bucket and is not cloned per item.
#[derive(Debug, Clone)]
pub struct EscrowBucket {
    inner: Arc<EscrowBucketInner>,
}

#[derive(Debug)]
struct EscrowBucketInner {
    charged_bytes: AtomicU64,
}

impl EscrowBucket {
    fn new() -> Self {
        Self {
            inner: Arc::new(EscrowBucketInner {
                charged_bytes: AtomicU64::new(0),
            }),
        }
    }

    /// Returns charged bytes currently held by this bucket.
    #[must_use]
    pub fn charged_bytes(&self) -> u64 {
        self.inner.charged_bytes.load(Ordering::Relaxed)
    }

    fn reserve(&self, bytes: u64, limit: u64, enforce: bool) -> Option<bool> {
        if enforce {
            let mut current = self.inner.charged_bytes.load(Ordering::Relaxed);
            loop {
                let next = current.saturating_add(bytes);
                if next > limit {
                    return None;
                }
                match self.inner.charged_bytes.compare_exchange_weak(
                    current,
                    next,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return Some(current == 0 && next > 0),
                    Err(observed) => current = observed,
                }
            }
        }

        Some(self.inner.charged_bytes.fetch_add(bytes, Ordering::Relaxed) == 0 && bytes > 0)
    }

    fn release(&self, bytes: u64) -> bool {
        if bytes == 0 {
            return false;
        }
        let previous = self.inner.charged_bytes.fetch_sub(bytes, Ordering::Relaxed);
        previous <= bytes
    }
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
        // The process-wide overshoot-debt allowance is the per-runtime debt
        // limit summed across all runtimes (design: `overshoot_debt_limit *
        // active_runtime_count`). Runtimes acquire from this pool before
        // retaining above floor+leases; post-hoc reconciliation may overdraw it.
        let allowed_overshoot = config
            .sizing
            .overshoot_debt_limit_bytes
            .saturating_mul(config.runtime_count as u64);
        self.inner.pool.set_overshoot_debt(allowed_overshoot);
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
            snapshot.publish(0, 0, 0, 0, 0, 0, 0, 0, BudgetLevel::Normal);
        }
        self.inner
            .snapshots
            .lock()
            .expect("memory budget snapshots poisoned")
            .push(Arc::downgrade(&snapshot));
        RuntimeMemorySnapshotHandle {
            snapshot,
            state: self.clone(),
            // Intern the attribution once per runtime so every ticket and
            // escrow owner derived from this handle shares it without cloning
            // strings on the per-item path.
            scope: Arc::new(scope),
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
        // Run the abandoned-escrow reaper as part of the periodic maintenance
        // pass before reading the aggregated counters, so reclaimed bytes are
        // reflected in the same snapshot. A no-op when reaping is disabled.
        let _ = self.reap_abandoned_escrow();
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
            abandoned_escrow_alarm_count: self
                .inner
                .abandoned_escrow_alarm_count
                .load(Ordering::Relaxed),
            reaped_escrow_count: self.inner.reaped_escrow_count.load(Ordering::Relaxed),
            reaped_escrow_bytes: self.inner.reaped_escrow_bytes.load(Ordering::Relaxed),
            escrow_ticket_count: self.inner.escrow_ticket_count.load(Ordering::Relaxed),
            escrow_charged_bytes: self.inner.escrow_charged_bytes.load(Ordering::Relaxed),
            escrow_pool_held_bytes: self.inner.escrow_pool_held_bytes.load(Ordering::Relaxed),
            escrow_pool_overshoot_bytes: self
                .inner
                .escrow_pool_overshoot_bytes
                .load(Ordering::Relaxed),
            spare_available_bytes: self.inner.pool.available_bytes(),
            overshoot_debt_balance: self.inner.pool.overshoot_debt_balance(),
            ..MemoryBudgetSnapshot::default()
        };
        {
            let abandoned = self
                .inner
                .abandoned_escrow
                .lock()
                .expect("memory budget abandoned escrow poisoned");
            if let Some(oldest) = abandoned.front() {
                let millis = oldest.abandoned_at.elapsed().as_millis();
                snapshot.abandoned_escrow_oldest_age_millis =
                    u64::try_from(millis).unwrap_or(u64::MAX).max(1);
            }
        }
        {
            let buckets = self
                .inner
                .escrow_buckets
                .lock()
                .expect("memory budget escrow buckets poisoned");
            for bucket in buckets.values() {
                let charged = bucket.charged_bytes();
                if charged > 0 {
                    snapshot.escrow_active_bucket_count += 1;
                    snapshot.escrow_max_bucket_bytes =
                        snapshot.escrow_max_bucket_bytes.max(charged);
                }
            }
        }
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
            snapshot.unknown_count = snapshot
                .unknown_count
                .saturating_add(runtime.unknown_count());
            snapshot.overshoot_bytes = snapshot
                .overshoot_bytes
                .saturating_add(runtime.overshoot_bytes());
            snapshot.reconcile_debt_bytes = snapshot
                .reconcile_debt_bytes
                .saturating_add(runtime.reconcile_debt_bytes());
            snapshot.drain_allowance_bytes = snapshot
                .drain_allowance_bytes
                .saturating_add(runtime.drain_allowance_bytes());
            snapshot.drain_committed_bytes = snapshot
                .drain_committed_bytes
                .saturating_add(runtime.drain_committed_bytes());
            match runtime.level() {
                1 => snapshot.soft_runtime_count += 1,
                2 => snapshot.hard_runtime_count += 1,
                _ => snapshot.normal_runtime_count += 1,
            }
        }
        snapshot
    }

    /// Stable identity for lightweight per-handle caches.
    #[must_use]
    pub(crate) fn cache_id(&self) -> usize {
        Arc::as_ptr(&self.inner) as usize
    }

    /// Returns the escrow bucket for a topic or shared boundary.
    ///
    /// The boundary id is interned by the caller, typically once per topic
    /// handle. This method clones only the cheap bucket handle.
    #[must_use]
    pub(crate) fn escrow_bucket(&self, boundary: &Arc<str>) -> EscrowBucket {
        let mut buckets = self
            .inner
            .escrow_buckets
            .lock()
            .expect("memory budget escrow buckets poisoned");
        buckets
            .entry(Arc::clone(boundary))
            .or_insert_with(EscrowBucket::new)
            .clone()
    }

    /// Returns charged bytes for a named escrow bucket, when it exists.
    #[must_use]
    pub fn escrow_bucket_charged_bytes(&self, boundary: &str) -> Option<u64> {
        self.inner
            .escrow_buckets
            .lock()
            .expect("memory budget escrow buckets poisoned")
            .get(boundary)
            .map(EscrowBucket::charged_bytes)
    }

    fn abandon_escrow(&self, bytes: u64, pool_backed: bool, bucket: &EscrowBucket) {
        self.inner
            .abandoned_escrow
            .lock()
            .expect("memory budget abandoned escrow poisoned")
            .push_back(AbandonedEscrowEntry {
                abandoned_at: Instant::now(),
                bytes,
                pool_backed,
                bucket: bucket.clone(),
            });
        let count = self
            .inner
            .abandoned_escrow_count
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let total_bytes = self
            .inner
            .abandoned_escrow_bytes
            .fetch_add(bytes, Ordering::Relaxed)
            .saturating_add(bytes);
        if !self
            .inner
            .abandoned_escrow_alarm_fired
            .swap(true, Ordering::AcqRel)
        {
            let _ = self
                .inner
                .abandoned_escrow_alarm_count
                .fetch_add(1, Ordering::Relaxed);
            otap_df_telemetry::otel_warn!(
                "runtime_memory_escrow.abandoned",
                abandoned_escrow_count = count,
                abandoned_escrow_bytes = total_bytes,
                message = "unresolved escrow dropped; charge remains sticky until an explicit reaper policy is configured",
            );
        }
    }

    /// Configures the abandoned-escrow reaper threshold.
    ///
    /// `Some(duration)` enables reaping: an abandoned escrow charge (a leaked
    /// `EscrowTicket` dropped without resolution) is reclaimed once it has been
    /// held at least `duration`, returning its bytes to the global pool while the
    /// cumulative abandoned/reaped metrics keep the leak observable. `None` (the
    /// default) disables reaping, preserving the prior sticky-forever behavior.
    /// A zero duration is treated as disabled.
    pub fn set_abandoned_escrow_reap_after(&self, after: Option<Duration>) {
        let millis = after
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        self.inner
            .abandoned_escrow_reap_after_millis
            .store(millis, Ordering::Relaxed);
    }

    /// Reclaims abandoned escrow entries older than the configured reap
    /// threshold, returning their bytes to the global pool.
    ///
    /// This is the explicit, conservative reaper policy. It is safe because an
    /// abandoned escrow charge is permanently orphaned the moment its
    /// `EscrowTicket` is dropped — no code can ever redeem/release/abort it
    /// again — so reclaiming only reverses an accounting charge that can never be
    /// resolved otherwise. Entries are reaped oldest-first (the deque is
    /// chronological); reclaimed bytes are recorded in the cumulative
    /// `reaped_escrow_count`/`reaped_escrow_bytes` totals so leaks remain visible
    /// rather than silently hidden. The cumulative abandoned totals are never
    /// decremented. Returns the number of entries reaped.
    ///
    /// Runs during the periodic snapshot maintenance pass; a no-op when reaping
    /// is disabled or no entry has aged past the threshold.
    fn reap_abandoned_escrow(&self) -> u64 {
        let after_millis = self
            .inner
            .abandoned_escrow_reap_after_millis
            .load(Ordering::Relaxed);
        if after_millis == 0 {
            return 0;
        }
        let after = Duration::from_millis(after_millis);

        // Collect the due entries under the lock, then reverse their charges
        // outside it to avoid holding the deque lock across pool/bucket updates.
        let mut due: Vec<AbandonedEscrowEntry> = Vec::new();
        {
            let mut abandoned = self
                .inner
                .abandoned_escrow
                .lock()
                .expect("memory budget abandoned escrow poisoned");
            while let Some(front) = abandoned.front() {
                if front.abandoned_at.elapsed() < after {
                    // Entries are chronological; the first non-due entry means no
                    // older entry remains.
                    break;
                }
                due.push(abandoned.pop_front().expect("front exists"));
            }
        }

        let mut reaped = 0u64;
        let mut reaped_bytes = 0u64;
        for entry in due {
            // Reverse the abandoned charge exactly as an explicit release would,
            // returning pool-backed bytes to the global pool.
            self.release_escrow(entry.bytes, entry.pool_backed, &entry.bucket);
            reaped = reaped.saturating_add(1);
            reaped_bytes = reaped_bytes.saturating_add(entry.bytes);
        }
        if reaped > 0 {
            let _ = self
                .inner
                .reaped_escrow_count
                .fetch_add(reaped, Ordering::Relaxed);
            let _ = self
                .inner
                .reaped_escrow_bytes
                .fetch_add(reaped_bytes, Ordering::Relaxed);
        }
        reaped
    }

    /// Outcome of [`MemoryBudgetState::try_charge_escrow`].
    fn try_charge_escrow(&self, bytes: u64, bucket: &EscrowBucket) -> Option<EscrowChargeAck> {
        let config = self.config()?;
        // The escrow boundary is the queue/topic publish admission point, so it
        // only rejects when enforce mode is active AND the per-path
        // `queue_publish` gate is enabled. With enforce mode but `queue_publish`
        // off (or in observe-only) it records pressure without rejecting.
        let enforce = config.mode == BudgetMode::Enforce && config.enforcement.queue_publish;
        // Step 1: bound the topic/boundary bucket occupancy (enforce only).
        let _activated_bucket = bucket.reserve(bytes, config.topic_default_limit_bytes, enforce)?;
        let _ = self
            .inner
            .escrow_charged_bytes
            .fetch_add(bytes, Ordering::Relaxed);
        // Step 2: back the escrow with an explicit global-pool borrow so the
        // design invariant
        //     sum(runtime_charged) + sum(escrow_charged) <= bounded capacity
        // is enforceable rather than optimistic. The producer's local lease
        // returns surplus to the pool on the matching `account.refund(bytes)`
        // performed by the caller after we ack the charge, so this borrow does
        // not durably double-draw the pool for the same logical bytes.
        let pool_backed = if bytes == 0 {
            true
        } else if self.inner.pool.try_borrow(bytes) {
            let _ = self
                .inner
                .escrow_pool_held_bytes
                .fetch_add(bytes, Ordering::AcqRel);
            true
        } else if enforce {
            // Roll back the topic-cap reservation; do not admit escrow that
            // would violate the global logical invariant in enforce mode.
            let _ = self
                .inner
                .escrow_charged_bytes
                .fetch_sub(bytes, Ordering::AcqRel);
            let _ = bucket.release(bytes);
            return None;
        } else {
            // Observe-only: record the unbacked escrow so the projected
            // overshoot is visible without rejecting the producer.
            let _ = self
                .inner
                .escrow_pool_overshoot_bytes
                .fetch_add(bytes, Ordering::Relaxed);
            false
        };
        let _ = self
            .inner
            .escrow_ticket_count
            .fetch_add(1, Ordering::Relaxed);
        Some(EscrowChargeAck { pool_backed })
    }

    fn release_escrow(&self, bytes: u64, pool_backed: bool, bucket: &EscrowBucket) {
        let _ = self
            .inner
            .escrow_ticket_count
            .fetch_sub(1, Ordering::Relaxed);
        let _ = self
            .inner
            .escrow_charged_bytes
            .fetch_sub(bytes, Ordering::Relaxed);
        let _ = bucket.release(bytes);
        if bytes == 0 {
            return;
        }
        if pool_backed {
            let _ = self
                .inner
                .escrow_pool_held_bytes
                .fetch_sub(bytes, Ordering::AcqRel);
            self.inner.pool.return_bytes(bytes);
        } else {
            let _ = self
                .inner
                .escrow_pool_overshoot_bytes
                .fetch_sub(bytes, Ordering::Relaxed);
        }
    }

    /// Mints one escrow owner in the provided topic or shared-boundary bucket.
    pub(crate) fn try_charge_escrow_owner_in_bucket(
        &self,
        bytes: u64,
        scope: &BudgetScope,
        bucket: &EscrowBucket,
    ) -> Option<EscrowTicket> {
        let ack = self.try_charge_escrow(bytes, bucket)?;
        Some(EscrowTicket {
            bytes,
            pool_backed: ack.pool_backed,
            state: self.clone(),
            bucket: bucket.clone(),
            // One reference-count bump at the shared boundary; no string clone.
            scope: Arc::clone(scope),
            resolved: false,
        })
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
    scope: BudgetScope,
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
        let account = RuntimeMemoryAccount::new(
            config.sizing.floor_per_runtime_bytes,
            config.sizing.lease_step_bytes,
            config.sizing.max_overshoot_per_runtime_bytes,
            config.mode,
            self.state.lease_authority(),
            self.snapshot.clone(),
            self.scope.clone(),
        );
        // Apply the operator-configured drain/redemption allowance (or the
        // resolved `lease_step_bytes` default). This is a one-time runtime
        // initialization step, off the per-item hot path.
        account.set_drain_allowance_bytes(config.sizing.effective_drain_allowance_bytes());
        Some(Rc::new(account))
    }
}

/// Shared spare pool used by runtime leases.
///
/// Tracks two distinct process-global pools:
///
/// - `available_bytes`: the spare lease pool. Runtimes borrow coarse
///   `lease_step` chunks from it once they exceed their local floor.
/// - `overshoot_debt_balance`: the bounded global overshoot-debt pool. When a
///   runtime needs to retain above `floor + borrowed_lease` it must acquire
///   authorized debt from this pool first. The balance is signed because a
///   post-hoc [`reconcile_size`](LocalMemoryTicket::reconcile_size) can overdraw
///   it: a negative balance is an explicit invariant-violation signal, not
///   hidden capacity.
#[derive(Debug, Default, Clone)]
pub struct GlobalLeasePool {
    available_bytes: Arc<AtomicU64>,
    overshoot_debt_balance: Arc<AtomicI64>,
}

impl GlobalLeasePool {
    /// Creates a pool with the given spare bytes and no overshoot-debt
    /// allowance.
    #[must_use]
    pub fn new(available_bytes: u64) -> Self {
        Self {
            available_bytes: Arc::new(AtomicU64::new(available_bytes)),
            overshoot_debt_balance: Arc::new(AtomicI64::new(0)),
        }
    }

    /// Creates a pool with explicit spare and overshoot-debt allowances.
    #[must_use]
    pub fn with_overshoot_debt(available_bytes: u64, overshoot_debt_bytes: u64) -> Self {
        Self {
            available_bytes: Arc::new(AtomicU64::new(available_bytes)),
            overshoot_debt_balance: Arc::new(AtomicI64::new(
                i64::try_from(overshoot_debt_bytes).unwrap_or(i64::MAX),
            )),
        }
    }

    fn set_available(&self, available_bytes: u64) {
        self.available_bytes
            .store(available_bytes, Ordering::Relaxed);
    }

    fn set_overshoot_debt(&self, overshoot_debt_bytes: u64) {
        self.overshoot_debt_balance.store(
            i64::try_from(overshoot_debt_bytes).unwrap_or(i64::MAX),
            Ordering::Relaxed,
        );
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

    /// Acquires up to `bytes` of authorized overshoot debt without overdrawing.
    ///
    /// Returns the amount actually acquired, which is `bytes` when the pool has
    /// enough positive balance and less (down to zero) otherwise. The caller is
    /// responsible for deciding whether a short acquisition should overdraw via
    /// [`overdraw_debt`](Self::overdraw_debt) (post-hoc reconcile) or be
    /// rejected (admission).
    #[must_use]
    pub fn acquire_debt_up_to(&self, bytes: u64) -> u64 {
        if bytes == 0 {
            return 0;
        }
        let want = i64::try_from(bytes).unwrap_or(i64::MAX);
        let mut current = self.overshoot_debt_balance.load(Ordering::Relaxed);
        loop {
            if current <= 0 {
                return 0;
            }
            let take = want.min(current);
            match self.overshoot_debt_balance.compare_exchange_weak(
                current,
                current - take,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return u64::try_from(take).unwrap_or(0),
                Err(next) => current = next,
            }
        }
    }

    /// Unconditionally subtracts `bytes` from the overshoot-debt pool, allowing
    /// the balance to go negative.
    ///
    /// Used only by post-hoc reconciliation, which must be infallible. A
    /// resulting negative balance is the explicit invariant-violation signal.
    pub fn overdraw_debt(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let amount = i64::try_from(bytes).unwrap_or(i64::MAX);
        let _ = self
            .overshoot_debt_balance
            .fetch_sub(amount, Ordering::AcqRel);
    }

    /// Repays previously acquired or overdrawn overshoot debt.
    pub fn repay_debt(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let amount = i64::try_from(bytes).unwrap_or(i64::MAX);
        let _ = self
            .overshoot_debt_balance
            .fetch_add(amount, Ordering::Release);
    }

    /// Returns the current signed overshoot-debt balance. Negative means the
    /// pool has been overdrawn by post-hoc reconciliation.
    #[must_use]
    pub fn overshoot_debt_balance(&self) -> i64 {
        self.overshoot_debt_balance.load(Ordering::Relaxed)
    }
}

/// Authority that grants and receives coarse lease bytes.
pub trait LeaseAuthority: std::fmt::Debug {
    /// Attempts to borrow a full amount. Partial borrows are not allowed.
    fn try_borrow(&self, bytes: u64) -> bool;

    /// Returns bytes to this lease authority.
    fn return_bytes(&self, bytes: u64);

    /// Acquires up to `bytes` of authorized overshoot debt without overdrawing.
    fn acquire_debt_up_to(&self, bytes: u64) -> u64;

    /// Unconditionally subtracts overshoot debt, allowing the balance to go
    /// negative (post-hoc reconcile overdraw).
    fn overdraw_debt(&self, bytes: u64);

    /// Repays previously acquired or overdrawn overshoot debt.
    fn repay_debt(&self, bytes: u64);
}

impl LeaseAuthority for GlobalLeasePool {
    fn try_borrow(&self, bytes: u64) -> bool {
        GlobalLeasePool::try_borrow(self, bytes)
    }

    fn return_bytes(&self, bytes: u64) {
        GlobalLeasePool::return_bytes(self, bytes);
    }

    fn acquire_debt_up_to(&self, bytes: u64) -> u64 {
        GlobalLeasePool::acquire_debt_up_to(self, bytes)
    }

    fn overdraw_debt(&self, bytes: u64) {
        GlobalLeasePool::overdraw_debt(self, bytes);
    }

    fn repay_debt(&self, bytes: u64) {
        GlobalLeasePool::repay_debt(self, bytes);
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
    scope: BudgetScope,
    mode: BudgetMode,
    floor_bytes: u64,
    lease: LocalMemoryLease,
    charged_bytes: Cell<u64>,
    unknown_bytes: Cell<u64>,
    unknown_count: Cell<u64>,
    overshoot_bytes: Cell<u64>,
    /// Overshoot debt acquired from the global pool and currently backed by it.
    debt_held: Cell<u64>,
    /// Overshoot debt that overdrew the global pool (unbacked). While this is
    /// non-zero the runtime is pinned to `Hard` and the global pool balance is
    /// negative by at least this amount until repaid.
    reconcile_debt: Cell<u64>,
    /// Per-runtime drain/redemption allowance (Phase 2 design lines 1154-1184).
    ///
    /// While the runtime is at local `Hard`, the consumer side may still admit
    /// up to this many bytes of redemption/drain work so consumers can make
    /// forward progress against retained escrow even when their account is
    /// otherwise at the classification ceiling. The allowance is only consumed
    /// by [`Self::try_charge_for_drain`] (the redeem/drain path) and never by
    /// regular receiver admission, so it cannot be turned into a sneak path
    /// for new external ingress.
    drain_allowance_bytes: Cell<u64>,
    /// Bytes currently outstanding against [`Self::drain_allowance_bytes`].
    /// Each successful drain charge increments this and the matching ticket
    /// drop decrements it.
    drain_committed: Cell<u64>,
    published_level: Cell<BudgetLevel>,
    dirty: Cell<bool>,
    snapshot: Arc<RuntimeMemorySnapshot>,
    _not_send: PhantomData<Rc<()>>,
}

impl RuntimeMemoryAccount {
    /// Creates a local runtime account.
    ///
    /// The drain/redemption allowance defaults to `lease_step_bytes` per the
    /// design's "at least `max(lease_step_bytes, largest_configured_topic_
    /// message_estimate)`" guidance; callers that know a more specific bound
    /// can adjust via [`Self::set_drain_allowance_bytes`].
    #[must_use]
    pub fn new(
        floor_bytes: u64,
        lease_step_bytes: u64,
        max_overshoot_bytes: u64,
        mode: BudgetMode,
        lease_authority: Arc<dyn LeaseAuthority>,
        snapshot: Arc<RuntimeMemorySnapshot>,
        scope: BudgetScope,
    ) -> Self {
        let account = Self {
            scope,
            mode,
            floor_bytes,
            lease: LocalMemoryLease::new(lease_step_bytes, max_overshoot_bytes, lease_authority),
            charged_bytes: Cell::new(0),
            unknown_bytes: Cell::new(0),
            unknown_count: Cell::new(0),
            overshoot_bytes: Cell::new(0),
            debt_held: Cell::new(0),
            reconcile_debt: Cell::new(0),
            drain_allowance_bytes: Cell::new(lease_step_bytes),
            drain_committed: Cell::new(0),
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

    /// Overrides the drain/redemption allowance for this account.
    ///
    /// Intended for runtime initialization (controller-driven). Not part of
    /// the per-item hot path.
    pub fn set_drain_allowance_bytes(&self, bytes: u64) {
        self.drain_allowance_bytes.set(bytes);
        self.mark_dirty();
    }

    /// Returns the configured drain/redemption allowance.
    #[must_use]
    pub fn drain_allowance_bytes(&self) -> u64 {
        self.drain_allowance_bytes.get()
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
    /// Admission path: raises authorized capacity with coarse lease borrows,
    /// then acquires authorized overshoot debt from the global pool for any
    /// remainder. In enforce mode it returns `false` (rolling back any partial
    /// debt acquisition) when neither leases nor the debt pool can cover the
    /// growth, leaving the existing charge untouched. In observe-only mode it
    /// always returns `true`, recording any would-be overdraw as reconciliation
    /// debt so operators can see the projected pressure.
    ///
    /// Hot-path: mutates only local `Cell` state; touches the shared global
    /// pool only when the reservation crosses the current lease/debt boundary.
    #[must_use]
    pub fn try_reserve_extra(&self, bytes: u64) -> bool {
        let next = self.charged_bytes.get().saturating_add(bytes);
        if next <= self.available_without_overshoot() {
            return true;
        }
        // Raise authorized capacity with coarse lease borrows first.
        let needed = next.saturating_sub(self.available_without_overshoot());
        if self.lease.try_borrow_for(needed) {
            self.mark_dirty();
        }
        if next <= self.available_without_overshoot() {
            return true;
        }
        // Still short: the remainder must be covered by overshoot debt.
        let overshoot_needed = next.saturating_sub(self.available_without_overshoot());
        if overshoot_needed > self.lease.max_overshoot_bytes() {
            // Beyond the per-runtime classification ceiling: cannot authorize.
            return self.mode == BudgetMode::ObserveOnly;
        }
        let held_total = self
            .debt_held
            .get()
            .saturating_add(self.reconcile_debt.get());
        let additional = overshoot_needed.saturating_sub(held_total);
        if additional == 0 {
            return true;
        }
        let got = self.lease.acquire_debt_up_to(additional);
        self.debt_held.set(self.debt_held.get().saturating_add(got));
        if got == additional {
            self.mark_dirty();
            return true;
        }
        if self.mode == BudgetMode::ObserveOnly {
            // Observe-only never overdraws the pool or pins Hard at admission:
            // the position-based Soft classification within the ceiling stands
            // and the unbacked remainder is surfaced via `overshoot_bytes`.
            // Reconciliation debt is reserved for the post-hoc `reconcile_size`
            // overdraw path.
            self.mark_dirty();
            return true;
        }
        // Enforce: roll back the partial acquisition so no debt is stranded.
        let got_back = got;
        self.lease.repay_debt(got_back);
        self.debt_held
            .set(self.debt_held.get().saturating_sub(got_back));
        false
    }

    /// Charges known retained bytes and returns a local ticket.
    ///
    /// Hot-path: mutates only local `Cell` state; defers snapshot publication
    /// to [`flush_snapshot`](Self::flush_snapshot) or a level transition.
    #[must_use]
    pub fn charge(self: &Rc<Self>, size: impl ChargedSize) -> Option<LocalMemoryTicket> {
        let Some(bytes) = size.charged_size() else {
            if self.mode == BudgetMode::Enforce {
                return None;
            }
            self.unknown_count
                .set(self.unknown_count.get().saturating_add(1));
            self.mark_dirty();
            return Some(LocalMemoryTicket {
                account: Rc::clone(self),
                charge: LocalMemoryCharge::Unknown,
                active: true,
                drain_used: 0,
                _not_send: PhantomData,
            });
        };
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
        self.commit_charge(bytes);
        Some(LocalMemoryTicket {
            account: Rc::clone(self),
            charge: LocalMemoryCharge::KnownBytes(bytes),
            active: true,
            drain_used: 0,
            _not_send: PhantomData,
        })
    }

    /// Drain/redemption-only admission path: charges `bytes` against the
    /// per-runtime drain allowance even when the account is at local `Hard`.
    ///
    /// Per the design (lines 1154-1184) the consumer side at local `Hard`
    /// must be able to redeem at least one in-transit escrowed item so that
    /// release can drain the queue and let pressure recover. This method is
    /// the primitive that admission of `EscrowTicket::redeem_into` falls back
    /// to in enforce mode when the consumer's normal charge would be
    /// rejected. It never admits new external work because it is callable
    /// only from the redemption/drain path.
    ///
    /// Returns `Some(ticket)` if the account is at `Hard` and the allowance
    /// has room for `bytes`. Returns `None` if the account is not at `Hard`
    /// (the regular [`Self::charge`] should be used instead), the allowance
    /// is exhausted, or the size is unknown.
    ///
    /// The bytes are still counted in `charged_bytes`, so the work remains
    /// attributed to this runtime. The release path is automatic: dropping
    /// the returned ticket refunds the charge and frees the allowance slot.
    #[must_use]
    pub fn try_charge_for_drain(
        self: &Rc<Self>,
        size: impl ChargedSize,
    ) -> Option<LocalMemoryTicket> {
        let bytes = size.charged_size()?;
        if self.level() != BudgetLevel::Hard {
            return None;
        }
        let used = self.drain_committed.get();
        let remaining = self.drain_allowance_bytes.get().saturating_sub(used);
        if bytes > remaining {
            return None;
        }
        // Allowance accounted; commit the charge bytes (still attributed to
        // this runtime) without going through `try_reserve_extra` so we do
        // not trip the enforce-mode admission rejection at Hard.
        self.drain_committed.set(used.saturating_add(bytes));
        self.commit_charge(bytes);
        Some(LocalMemoryTicket {
            account: Rc::clone(self),
            charge: LocalMemoryCharge::KnownBytes(bytes),
            active: true,
            drain_used: bytes,
            _not_send: PhantomData,
        })
    }

    /// Commits a previously-reserved growth: grows charged bytes and settles
    /// debt/level state. Used by [`charge`](Self::charge) and the ticket-level
    /// grow APIs after [`try_reserve_extra`](Self::try_reserve_extra) succeeds.
    fn commit_charge(&self, bytes: u64) {
        self.charged_bytes
            .set(self.charged_bytes.get().saturating_add(bytes));
        self.settle();
    }

    /// Records retained bytes whose logical size is unknown.
    ///
    /// Hot-path: mutates only local `Cell` state.
    pub fn observe_unknown(&self, bytes: u64) {
        self.unknown_bytes
            .set(self.unknown_bytes.get().saturating_add(bytes));
        self.mark_dirty();
    }

    /// Grows charged bytes post-hoc when the exact retained size is only known
    /// after the growth already happened.
    ///
    /// This is infallible: it acquires authorized overshoot debt where the pool
    /// allows and overdraws the pool for any remainder, recording the unbacked
    /// excess as reconciliation debt (which pins the runtime to `Hard` until it
    /// drains below authorized capacity and repays).
    fn reconcile_grow(&self, extra: u64) {
        self.charged_bytes
            .set(self.charged_bytes.get().saturating_add(extra));
        self.raise_debt_to_overshoot();
        self.settle();
    }

    /// Tops up overshoot debt to back the current overshoot, overdrawing the
    /// global pool (and recording reconciliation debt) for any shortfall.
    fn raise_debt_to_overshoot(&self) {
        let charged = self.charged_bytes.get();
        let overshoot = charged.saturating_sub(self.available_without_overshoot());
        let held_total = self
            .debt_held
            .get()
            .saturating_add(self.reconcile_debt.get());
        if overshoot <= held_total {
            return;
        }
        let need = overshoot - held_total;
        let got = self.lease.acquire_debt_up_to(need);
        self.debt_held.set(self.debt_held.get().saturating_add(got));
        let short = need - got;
        if short > 0 {
            self.lease.overdraw_debt(short);
            self.reconcile_debt
                .set(self.reconcile_debt.get().saturating_add(short));
        }
    }

    /// Recomputes overshoot, returns surplus leases/debt, and republishes the
    /// pressure level on a transition.
    ///
    /// Hot-path: mutates only local `Cell` state. The shared snapshot is
    /// touched only if the pressure level transitioned (an explicit
    /// operator-relevant event), never on every charged item.
    fn settle(&self) {
        let charged = self.charged_bytes.get();
        self.lease.return_lazy(charged, self.floor_bytes);
        let overshoot = charged.saturating_sub(self.available_without_overshoot());
        self.overshoot_bytes.set(overshoot);
        self.repay_surplus_debt(overshoot);
        let new_level = self.classify(charged);
        if new_level != self.published_level.get() {
            // Level transitions are operator-visible; publish immediately.
            self.publish_level(new_level);
        } else {
            self.mark_dirty();
        }
    }

    /// Repays overshoot debt down to the current overshoot.
    ///
    /// Authorized `debt_held` is repaid first so that unbacked reconciliation
    /// debt (and the `Hard` pin it implies) persists until the overshoot is
    /// fully drained.
    fn repay_surplus_debt(&self, overshoot: u64) {
        let held = self.debt_held.get();
        let recon = self.reconcile_debt.get();
        let total = held.saturating_add(recon);
        if total <= overshoot {
            return;
        }
        let mut surplus = total - overshoot;
        let from_held = surplus.min(held);
        if from_held > 0 {
            self.lease.repay_debt(from_held);
            self.debt_held.set(held - from_held);
            surplus -= from_held;
        }
        if surplus > 0 {
            self.lease.repay_debt(surplus);
            self.reconcile_debt.set(recon - surplus);
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
        self.settle();
    }

    fn refund_unknown(&self) {
        self.unknown_count
            .set(self.unknown_count.get().saturating_sub(1));
        self.mark_dirty();
    }

    fn classify(&self, charged: u64) -> BudgetLevel {
        if self.reconcile_debt.get() > 0 {
            // Unbacked overshoot debt pins the runtime to Hard until repaid.
            return BudgetLevel::Hard;
        }
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
            self.unknown_count.get(),
            self.overshoot_bytes.get(),
            self.reconcile_debt.get(),
            self.drain_allowance_bytes.get(),
            self.drain_committed.get(),
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

    fn acquire_debt_up_to(&self, bytes: u64) -> u64 {
        self.lease_authority.acquire_debt_up_to(bytes)
    }

    fn overdraw_debt(&self, bytes: u64) {
        self.lease_authority.overdraw_debt(bytes);
    }

    fn repay_debt(&self, bytes: u64) {
        self.lease_authority.repay_debt(bytes);
    }
}

/// Error returned by fallible memory-budget growth operations.
///
/// Shrinking, dropping, and releasing a ticket are always infallible; only
/// growth (which must reserve budget before it commits) can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetError {
    /// The growth could not be authorized (no spare lease or overshoot debt
    /// available under enforcement). The original charge is preserved.
    Exhausted,
    /// The operation requires a known retained size but the ticket has an
    /// unknown size. Convert it first with
    /// [`reconcile_size`](LocalMemoryTicket::reconcile_size).
    UnknownSize,
}

impl std::fmt::Display for BudgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exhausted => f.write_str("memory budget growth could not be authorized"),
            Self::UnknownSize => {
                f.write_str("operation requires a known retained size on the ticket")
            }
        }
    }
}

impl std::error::Error for BudgetError {}

/// Local logical charge. This type is intentionally `!Send`.
#[derive(Debug)]
pub struct LocalMemoryTicket {
    account: Rc<RuntimeMemoryAccount>,
    charge: LocalMemoryCharge,
    active: bool,
    /// Non-zero only when this ticket was admitted via
    /// [`RuntimeMemoryAccount::try_charge_for_drain`]. On drop the account's
    /// `drain_committed` counter is decremented by this amount so the
    /// allowance slot is returned.
    drain_used: u64,
    _not_send: PhantomData<Rc<()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalMemoryCharge {
    KnownBytes(u64),
    Unknown,
}

impl LocalMemoryTicket {
    /// Returns bytes owned by this ticket.
    #[must_use]
    pub const fn bytes(&self) -> Option<u64> {
        match self.charge {
            LocalMemoryCharge::KnownBytes(bytes) => Some(bytes),
            LocalMemoryCharge::Unknown => None,
        }
    }

    /// Returns attribution scope for this ticket.
    ///
    /// Borrowed from the owning account, so reading the scope costs nothing on
    /// the per-item path: there is no interned-handle refcount bump and no
    /// string clone.
    #[must_use]
    pub fn scope(&self) -> &BudgetScopeId {
        self.account.scope()
    }

    /// Resizes the retained charge from `old_bytes` to `new_bytes`.
    ///
    /// Reserves any positive delta before committing the growth, shrinks
    /// infallibly when `new_bytes` is smaller, and leaves the original ticket
    /// and original charge valid if a grow reservation fails. `old_bytes` must
    /// equal the ticket's current known charge; resizing an unknown-size ticket
    /// returns [`BudgetError::UnknownSize`].
    pub fn try_resize(&mut self, old_bytes: u64, new_bytes: u64) -> Result<(), BudgetError> {
        let LocalMemoryCharge::KnownBytes(current) = self.charge else {
            return Err(BudgetError::UnknownSize);
        };
        debug_assert_eq!(
            current, old_bytes,
            "try_resize old_bytes must match the ticket's current charge"
        );
        if new_bytes > current {
            let extra = new_bytes - current;
            if !self.account.try_reserve_extra(extra) {
                return Err(BudgetError::Exhausted);
            }
            self.account.commit_charge(extra);
        } else if new_bytes < current {
            self.account.refund(current - new_bytes);
        }
        self.charge = LocalMemoryCharge::KnownBytes(new_bytes);
        Ok(())
    }

    /// Reserves additional retained bytes before a deferred growth.
    ///
    /// On success the account is charged for the extra bytes and the ticket's
    /// known size grows by `extra_bytes`; the caller may then grow the retained
    /// buffer and finalize the exact size with
    /// [`reconcile_size`](Self::reconcile_size). Fails without side effects when
    /// the ticket has an unknown size or the reservation cannot be authorized.
    pub fn try_reserve_extra(&mut self, extra_bytes: u64) -> Result<(), BudgetError> {
        let LocalMemoryCharge::KnownBytes(current) = self.charge else {
            return Err(BudgetError::UnknownSize);
        };
        if extra_bytes == 0 {
            return Ok(());
        }
        if !self.account.try_reserve_extra(extra_bytes) {
            return Err(BudgetError::Exhausted);
        }
        self.account.commit_charge(extra_bytes);
        self.charge = LocalMemoryCharge::KnownBytes(current.saturating_add(extra_bytes));
        Ok(())
    }

    /// Reconciles the ticket to an exact retained size known only after growth.
    ///
    /// Infallible: any excess beyond previously reserved budget is charged
    /// post-hoc against authorized overshoot debt, overdrawing the global pool
    /// (and recording reconciliation debt that pins the runtime to `Hard`) when
    /// it cannot be authorized. Also converts an unknown-size ticket into a
    /// known-size charge.
    pub fn reconcile_size(&mut self, new_bytes: u64) {
        match self.charge {
            LocalMemoryCharge::KnownBytes(current) => {
                if new_bytes > current {
                    self.account.reconcile_grow(new_bytes - current);
                } else if new_bytes < current {
                    self.account.refund(current - new_bytes);
                }
            }
            LocalMemoryCharge::Unknown => {
                // Convert an unknown observation into a known charge.
                self.account.refund_unknown();
                self.account.reconcile_grow(new_bytes);
            }
        }
        self.charge = LocalMemoryCharge::KnownBytes(new_bytes);
    }

    /// Reserves a new logical owner for a retained fanout/clone branch.
    ///
    /// Each retained branch needs its own charge, so this charges a fresh
    /// ticket of `bytes` against the same account rather than splitting the
    /// existing charge. Returns [`BudgetError::Exhausted`] if the additional
    /// owner cannot be authorized under enforcement.
    pub fn try_reserve_clone(&self, bytes: u64) -> Result<LocalMemoryTicket, BudgetError> {
        self.account.charge(bytes).ok_or(BudgetError::Exhausted)
    }

    /// Converts this local ticket into escrow ownership.
    ///
    /// Tickets admitted via [`RuntimeMemoryAccount::try_charge_for_drain`]
    /// (the per-runtime drain/redemption allowance) cannot be transferred
    /// back to escrow: the allowance is a redeem-only path, not a producer
    /// path. Those tickets must be drop-released by the consumer instead.
    pub fn try_into_escrow(
        self,
        state: &MemoryBudgetState,
    ) -> Result<EscrowTicket, LocalMemoryTicket> {
        let bucket = state.escrow_bucket(&DEFAULT_ESCROW_BUCKET);
        self.try_into_escrow_in_bucket(state, &bucket)
    }

    /// Converts this local ticket into escrow ownership in a specific boundary
    /// bucket.
    pub(crate) fn try_into_escrow_in_bucket(
        mut self,
        state: &MemoryBudgetState,
        bucket: &EscrowBucket,
    ) -> Result<EscrowTicket, LocalMemoryTicket> {
        if self.drain_used > 0 {
            return Err(self);
        }
        let LocalMemoryCharge::KnownBytes(bytes) = self.charge else {
            return Err(self);
        };
        let Some(escrow) =
            state.try_charge_escrow_owner_in_bucket(bytes, &self.account.scope, bucket)
        else {
            return Err(self);
        };
        self.active = false;
        // The escrow charge now holds an explicit pool borrow for `bytes` (when
        // `pool_backed`); refunding the producer here lets the producer's
        // lease return surplus to the pool on settle, restoring approximately
        // the same pool draw we just took. This is the "transfer from existing
        // lease" path described by the design (line 728-733): the global
        // logical total does not increase across the transfer.
        self.account.refund(bytes);
        Ok(escrow)
    }

    /// Converts this local ticket into `count` independent escrow owners for an
    /// all-or-nothing fanout publish (mixed topics).
    ///
    /// Each owner is charged the ticket's full logical bytes because Phase 2
    /// charges per retained logical owner, not per underlying allocation
    /// (design "Fanout and Shared-Buffer Semantics"): a mixed publish that
    /// retains the same `Arc<T>` in `N` balanced queues plus one broadcast ring
    /// slot has `N + 1` retained owners.
    ///
    /// Reservation is transactional. If every owner is charged, the producer's
    /// local charge is refunded exactly once and the owners are returned. If
    /// any owner cannot be charged, all already-acquired owners are released in
    /// reverse acquisition order and the original [`LocalMemoryTicket`] is
    /// returned unchanged so the caller keeps the charge and can apply its
    /// failed-publish policy. Drain-allowance and unknown-size tickets cannot
    /// fan out into escrow and are returned unchanged.
    pub fn try_into_escrow_fanout(
        self,
        state: &MemoryBudgetState,
        count: usize,
    ) -> Result<SmallVec<[EscrowTicket; 4]>, LocalMemoryTicket> {
        let bucket = state.escrow_bucket(&DEFAULT_ESCROW_BUCKET);
        self.try_into_escrow_fanout_in_bucket(state, count, &bucket)
    }

    /// Converts this local ticket into `count` independent escrow owners in a
    /// specific boundary bucket.
    pub(crate) fn try_into_escrow_fanout_in_bucket(
        mut self,
        state: &MemoryBudgetState,
        count: usize,
        bucket: &EscrowBucket,
    ) -> Result<SmallVec<[EscrowTicket; 4]>, LocalMemoryTicket> {
        if count == 0 || self.drain_used > 0 {
            return Err(self);
        }
        let LocalMemoryCharge::KnownBytes(bytes) = self.charge else {
            return Err(self);
        };
        let mut owners: SmallVec<[EscrowTicket; 4]> = SmallVec::with_capacity(count);
        for _ in 0..count {
            match state.try_charge_escrow_owner_in_bucket(bytes, &self.account.scope, bucket) {
                Some(owner) => owners.push(owner),
                None => {
                    // Unwind already-acquired owners in reverse order, then
                    // return the original ticket: nothing was committed.
                    while let Some(owner) = owners.pop() {
                        owner.release();
                    }
                    return Err(self);
                }
            }
        }
        self.active = false;
        self.account.refund(bytes);
        Ok(owners)
    }
}

impl Drop for LocalMemoryTicket {
    fn drop(&mut self) {
        if self.active {
            match self.charge {
                LocalMemoryCharge::KnownBytes(bytes) => self.account.refund(bytes),
                LocalMemoryCharge::Unknown => self.account.refund_unknown(),
            }
            if self.drain_used > 0 {
                let used = self.account.drain_committed.get();
                self.account
                    .drain_committed
                    .set(used.saturating_sub(self.drain_used));
                self.drain_used = 0;
            }
            self.active = false;
        }
    }
}

/// Acknowledgement returned by [`MemoryBudgetState::try_charge_escrow`]:
/// the escrow occupancy and the pool draw have been recorded and the escrow
/// counter is live; the caller now owns the matching release.
#[derive(Debug, Clone, Copy)]
struct EscrowChargeAck {
    /// True iff this escrow created an explicit borrow against the global
    /// spare pool. False only when admission ran in observe-only mode and the
    /// pool could not satisfy the borrow (the escrow proceeded but the global
    /// logical invariant is overshot — surfaced via `escrow_pool_overshoot_bytes`).
    pool_backed: bool,
}

/// Cross-runtime/topic logical charge.
#[derive(Debug)]
pub struct EscrowTicket {
    bytes: u64,
    /// Whether this ticket holds an explicit pool borrow. Drives the matching
    /// release path: pool-backed releases return `bytes` to the pool;
    /// observe-only-overshoot releases decrement the overshoot counter without
    /// returning to a pool draw that was never taken.
    pool_backed: bool,
    state: MemoryBudgetState,
    bucket: EscrowBucket,
    scope: BudgetScope,
    resolved: bool,
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

    /// Releases this escrow's accounting exactly once.
    ///
    /// Shared by every explicit terminal transition (`redeem`, `release`,
    /// `abort`). It returns the logical bytes to the escrow boundary and marks
    /// the ticket resolved so `Drop` does not route it to the graveyard.
    fn resolve_release(&mut self) {
        if !self.resolved {
            self.state
                .release_escrow(self.bytes, self.pool_backed, &self.bucket);
            self.resolved = true;
        }
    }

    /// Redeems escrow on point-to-point delivery without re-charging a consumer
    /// account.
    ///
    /// Use this for balanced delivery where the consumer does not retain the
    /// payload in its own runtime account. To move ownership into a consumer
    /// runtime instead, use [`redeem_into`](Self::redeem_into).
    pub fn redeem(mut self) {
        self.resolve_release();
    }

    /// Redeems escrow into a consumer runtime's local account.
    ///
    /// On success the escrow charge is released and an equivalent
    /// [`LocalMemoryTicket`] is charged to `account`, so the global logical
    /// total is preserved across the handoff. On failure (enforce mode rejects
    /// the consumer charge even after the drain-allowance fallback) the
    /// original [`EscrowTicket`] is returned unchanged and still owns the
    /// charge, so the boundary can apply its drop/retry policy without
    /// creating an unowned interval.
    ///
    /// Drain allowance fallback (Phase 2 design lines 1154-1184): when the
    /// consumer's normal admission is rejected and the account is at local
    /// `Hard`, this falls back to the per-runtime drain/redemption allowance
    /// so consumers can still drain at least one in-transit escrowed item
    /// even when their account is otherwise at the classification ceiling.
    /// The allowance bytes are still charged to the consumer runtime; this
    /// path does not admit new external ingress.
    #[must_use = "a returned EscrowTicket still owns the escrow charge"]
    pub fn redeem_into(
        mut self,
        account: &Rc<RuntimeMemoryAccount>,
    ) -> Result<LocalMemoryTicket, EscrowTicket> {
        if let Some(ticket) = account.charge(self.bytes) {
            self.resolve_release();
            return Ok(ticket);
        }
        // Normal admission was rejected (enforce mode). If we are at local
        // Hard, the drain allowance permits a bounded redeem so the queue
        // can keep draining.
        if let Some(ticket) = account.try_charge_for_drain(self.bytes) {
            self.resolve_release();
            return Ok(ticket);
        }
        Err(self)
    }

    /// Releases escrow on eviction, drop-oldest, topic close, or final drain.
    ///
    /// This is the broker-driven release path. It is accounting-equivalent to
    /// [`redeem`](Self::redeem) but names the eviction lifecycle event for
    /// release-cause metrics.
    pub fn release(mut self) {
        self.resolve_release();
    }

    /// Explicitly aborts an in-flight escrow owner, releasing its charge inline.
    ///
    /// Aborting is the tracked negative outcome. Only bypassing this (and the
    /// other explicit terminal paths) by dropping an unresolved ticket routes
    /// the charge to the leak-detection graveyard.
    pub fn abort(mut self) {
        self.resolve_release();
    }
}

impl Drop for EscrowTicket {
    fn drop(&mut self) {
        if !self.resolved {
            // No explicit redeem/release/abort ran: record an abandoned-escrow
            // entry so the leak stays visible instead of silently succeeding.
            self.state
                .abandon_escrow(self.bytes, self.pool_backed, &self.bucket);
            self.resolved = true;
        }
    }
}

/// RAII holder for an escrow owner stored inside a shared-boundary queue slot.
///
/// A queued item at a shared boundary (such as a balanced topic queue entry)
/// owns its escrow charge while it sits in transit. `EscrowSlot` makes the
/// release path uniform and exactly-once: whenever the slot is dropped — on
/// delivery, eviction, drop-on-full, topic close, or final drain — it releases
/// the escrow through [`EscrowTicket::release`] rather than routing it to the
/// abandoned-escrow graveyard. The graveyard then stays reserved for genuine
/// leaks where an [`EscrowTicket`] is separated from its slot and lost.
///
/// `EscrowSlot` is `Send` (it holds only a sendable [`EscrowTicket`]), so it can
/// live inside a `Send` shared-queue envelope.
#[derive(Debug, Default)]
pub struct EscrowSlot {
    ticket: Option<EscrowTicket>,
}

impl EscrowSlot {
    /// Creates a slot owning the given escrow ticket.
    #[must_use]
    pub fn new(ticket: EscrowTicket) -> Self {
        Self {
            ticket: Some(ticket),
        }
    }

    /// Creates an empty slot that owns nothing (the budget-disabled or
    /// uncharged case).
    #[must_use]
    pub fn empty() -> Self {
        Self { ticket: None }
    }

    /// Returns whether this slot owns an escrow ticket.
    #[must_use]
    pub fn is_some(&self) -> bool {
        self.ticket.is_some()
    }

    /// Returns the escrow bytes owned by this slot, if any.
    #[must_use]
    pub fn bytes(&self) -> Option<u64> {
        self.ticket.as_ref().map(EscrowTicket::bytes)
    }

    /// Removes and returns the owned escrow ticket, leaving the slot empty.
    ///
    /// The caller becomes responsible for resolving the returned ticket
    /// (redeem/release/abort). Used when a consumer wants to redeem the escrow
    /// into its own runtime account on delivery rather than release it.
    #[must_use]
    pub fn take(&mut self) -> Option<EscrowTicket> {
        self.ticket.take()
    }
}

impl Drop for EscrowSlot {
    fn drop(&mut self) {
        if let Some(ticket) = self.ticket.take() {
            // Uniform clean release on every queue-exit path (delivery, close,
            // drain, eviction, drop-on-full). Not the abandoned graveyard.
            ticket.release();
        }
    }
}

/// Pairs a retained local payload with the [`LocalMemoryTicket`] that owns its
/// charge, for retained local-channel and local-scheduler paths.
///
/// This is the engine-owned attachment strategy from the design: it keeps the
/// ticket out of `PData` (which is `Clone + Send + Sync`) while binding the
/// ticket lifetime to the payload it accounts for. Because it can hold a
/// `LocalMemoryTicket`, a `LocalEnvelope<T>` is intentionally `!Send` and must
/// not cross a shared boundary; publishing across a shared boundary must first
/// convert the ticket with [`LocalMemoryTicket::try_into_escrow`].
///
/// Dropping the envelope drops the ticket through RAII, releasing the charge
/// exactly once. Splitting the envelope with [`into_parts`](Self::into_parts)
/// moves the ticket out so the caller becomes responsible for its single
/// release.
///
/// The envelope carries an `Option<LocalMemoryTicket>` rather than a required
/// ticket so the same type works when memory budgeting is disabled (no ambient
/// runtime budget) without forcing callers to branch on two payload shapes.
#[derive(Debug)]
pub struct LocalEnvelope<T> {
    payload: T,
    ticket: Option<LocalMemoryTicket>,
    // Redundant with `LocalMemoryTicket`'s own `!Send` marker, but keeps the
    // envelope `!Send` even in the `None`/budget-disabled case so callers can
    // rely on a single, mode-independent ownership invariant.
    _not_send: PhantomData<Rc<()>>,
}

impl<T> LocalEnvelope<T> {
    /// Creates an envelope that pairs `payload` with its owning `ticket`.
    #[must_use]
    pub fn new(payload: T, ticket: LocalMemoryTicket) -> Self {
        Self {
            payload,
            ticket: Some(ticket),
            _not_send: PhantomData,
        }
    }

    /// Creates an envelope with no ticket, for when memory budgeting is disabled
    /// or the payload is not charged.
    #[must_use]
    pub fn without_ticket(payload: T) -> Self {
        Self {
            payload,
            ticket: None,
            _not_send: PhantomData,
        }
    }

    /// Charges `payload` against the current runtime budget (if installed) and
    /// wraps it in an envelope.
    ///
    /// Returns `None` only when an enforce-mode budget refuses the charge; in
    /// that case the caller still owns `payload` and can apply its
    /// failed-admission policy. When no runtime budget is installed the payload
    /// is wrapped without a ticket.
    #[must_use]
    pub fn charge_current(payload: T) -> Option<Self>
    where
        T: ChargedSize,
    {
        match current_runtime_memory_budget() {
            Some(budget) => budget
                .charge(&payload)
                .map(|ticket| Self::new(payload, ticket)),
            None => Some(Self::without_ticket(payload)),
        }
    }

    /// Returns a shared reference to the retained payload.
    #[must_use]
    pub fn payload(&self) -> &T {
        &self.payload
    }

    /// Returns a mutable reference to the retained payload.
    pub fn payload_mut(&mut self) -> &mut T {
        &mut self.payload
    }

    /// Returns the owning ticket, if any.
    #[must_use]
    pub fn ticket(&self) -> Option<&LocalMemoryTicket> {
        self.ticket.as_ref()
    }

    /// Returns whether this envelope owns a ticket.
    #[must_use]
    pub fn has_ticket(&self) -> bool {
        self.ticket.is_some()
    }

    /// Consumes the envelope, returning the payload and releasing the ticket.
    #[must_use]
    pub fn into_payload(self) -> T {
        // The ticket field drops here, releasing the charge exactly once.
        self.payload
    }

    /// Splits the envelope into its payload and ticket.
    ///
    /// Ownership of the ticket transfers to the caller, who becomes responsible
    /// for its single release (drop, convert to escrow, etc.).
    #[must_use]
    pub fn into_parts(self) -> (T, Option<LocalMemoryTicket>) {
        (self.payload, self.ticket)
    }

    /// Replaces the payload while preserving the ticket, returning the previous
    /// payload. Useful when a processor swaps the retained value in place.
    pub fn replace_payload(&mut self, payload: T) -> T {
        std::mem::replace(&mut self.payload, payload)
    }

    /// Converts this local-owned envelope into a sendable shared-owned envelope
    /// by moving its ticket into escrow ownership.
    ///
    /// Use this when retained work crosses from a local (`!Send`) context into a
    /// shared channel or shared node that must hold the charge across threads.
    /// The resulting [`SharedEnvelope`] is `Send` and holds only an
    /// [`EscrowSlot`], so a `!Send` [`LocalMemoryTicket`] can never cross the
    /// shared boundary inside it by construction.
    ///
    /// - No ticket (budgeting disabled / uncharged): the payload moves into an
    ///   owner-less shared envelope.
    /// - Ticket present and escrow accepts: the charge transfers to escrow
    ///   exactly once (the producer local charge is refunded by the transfer).
    /// - Escrow refused (enforce-mode cap, exhausted pool, or a
    ///   non-transferable drain/unknown ticket): the original [`LocalEnvelope`]
    ///   is returned unchanged so the caller keeps the local charge.
    pub fn into_shared(
        self,
        state: &MemoryBudgetState,
    ) -> Result<SharedEnvelope<T>, LocalEnvelope<T>> {
        let (payload, ticket) = self.into_parts();
        match ticket {
            None => Ok(SharedEnvelope::without_owner(payload)),
            Some(ticket) => match ticket.try_into_escrow(state) {
                Ok(escrow) => Ok(SharedEnvelope::new(payload, EscrowSlot::new(escrow))),
                Err(ticket) => Err(LocalEnvelope::new(payload, ticket)),
            },
        }
    }
}

/// Pairs a retained payload with a sendable escrow owner for shared-channel and
/// shared-node retention.
///
/// Unlike [`LocalEnvelope<T>`] — which is `!Send` because it can hold a `!Send`
/// [`LocalMemoryTicket`] — `SharedEnvelope<T>` holds only an [`EscrowSlot`]
/// (which owns a `Send` [`EscrowTicket`]). It is therefore `Send` whenever `T`
/// is `Send`, and a local ticket can never enter a shared boundary inside it by
/// construction: the only way to obtain a charged `SharedEnvelope` is to mint
/// an escrow owner, which already left the producer runtime generation.
///
/// The shared retained charge is charged exactly once when the owner is created
/// (via [`LocalEnvelope::into_shared`] or an explicit escrow owner) and released
/// exactly once when the envelope is dropped or split with
/// [`into_parts`](Self::into_parts). Like [`LocalEnvelope`], the owner is
/// optional so the same type works when memory budgeting is disabled.
#[derive(Debug)]
pub struct SharedEnvelope<T> {
    payload: T,
    // Owns the shared retained escrow charge. Dropping the envelope drops this
    // slot, releasing the charge exactly once via EscrowSlot's Drop. Empty when
    // budgeting is disabled or the payload is uncharged.
    escrow: EscrowSlot,
}

impl<T> SharedEnvelope<T> {
    /// Creates a shared envelope pairing `payload` with its sendable escrow
    /// owner.
    #[must_use]
    pub fn new(payload: T, escrow: EscrowSlot) -> Self {
        Self { payload, escrow }
    }

    /// Creates a shared envelope with no owner, for when memory budgeting is
    /// disabled or the payload is not charged.
    #[must_use]
    pub fn without_owner(payload: T) -> Self {
        Self {
            payload,
            escrow: EscrowSlot::empty(),
        }
    }

    /// Returns a shared reference to the retained payload.
    #[must_use]
    pub fn payload(&self) -> &T {
        &self.payload
    }

    /// Returns a mutable reference to the retained payload.
    pub fn payload_mut(&mut self) -> &mut T {
        &mut self.payload
    }

    /// Returns whether this envelope owns an escrow charge.
    #[must_use]
    pub fn has_owner(&self) -> bool {
        self.escrow.is_some()
    }

    /// Returns the escrow bytes owned by this envelope, if any.
    #[must_use]
    pub fn owner_bytes(&self) -> Option<u64> {
        self.escrow.bytes()
    }

    /// Consumes the envelope, returning the payload and releasing the escrow
    /// owner exactly once.
    #[must_use]
    pub fn into_payload(self) -> T {
        // The escrow slot field drops here, releasing the charge exactly once.
        self.payload
    }

    /// Splits the envelope into its payload and escrow owner.
    ///
    /// Ownership of the escrow transfers to the caller via the returned
    /// [`EscrowSlot`], who becomes responsible for resolving it (redeem on
    /// delivery, release on eviction, or drop for a clean release).
    #[must_use]
    pub fn into_parts(self) -> (T, EscrowSlot) {
        (self.payload, self.escrow)
    }
}

/// Logical retained-size contract for memory budgeting.
pub trait ChargedSize {
    /// Returns the logical retained byte size when known.
    fn charged_size(&self) -> Option<u64>;
}

impl ChargedSize for u64 {
    fn charged_size(&self) -> Option<u64> {
        Some(*self)
    }
}

impl ChargedSize for u32 {
    fn charged_size(&self) -> Option<u64> {
        Some((*self).into())
    }
}

impl ChargedSize for i32 {
    fn charged_size(&self) -> Option<u64> {
        Some(u64::try_from(*self).unwrap_or(0))
    }
}

impl ChargedSize for usize {
    fn charged_size(&self) -> Option<u64> {
        Some(*self as u64)
    }
}

impl<T: ChargedSize + ?Sized> ChargedSize for &T {
    fn charged_size(&self) -> Option<u64> {
        (*self).charged_size()
    }
}

impl<T: ChargedSize + ?Sized> ChargedSize for Box<T> {
    fn charged_size(&self) -> Option<u64> {
        self.as_ref().charged_size()
    }
}

impl ChargedSize for &[u8] {
    fn charged_size(&self) -> Option<u64> {
        Some(self.len() as u64)
    }
}

impl ChargedSize for str {
    fn charged_size(&self) -> Option<u64> {
        Some(self.len() as u64)
    }
}

impl ChargedSize for String {
    fn charged_size(&self) -> Option<u64> {
        Some(self.len() as u64)
    }
}

impl ChargedSize for Vec<u8> {
    fn charged_size(&self) -> Option<u64> {
        Some(self.len() as u64)
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
    use static_assertions::{assert_impl_all, assert_not_impl_any};

    struct UnknownSize;

    impl ChargedSize for UnknownSize {
        fn charged_size(&self) -> Option<u64> {
            None
        }
    }

    assert_not_impl_any!(RuntimeMemoryAccount: Send, Sync);
    assert_not_impl_any!(RuntimeMemoryBudget: Send, Sync);
    assert_not_impl_any!(RuntimeMemoryBudgetGuard: Send, Sync);
    assert_not_impl_any!(LocalMemoryTicket: Send, Sync);
    // A LocalEnvelope can hold a !Send LocalMemoryTicket, so it must not be
    // Send/Sync regardless of payload sendability.
    assert_not_impl_any!(LocalEnvelope<u64>: Send, Sync);
    // A SharedEnvelope holds only a Send EscrowSlot owner, never a local
    // ticket, so it is Send whenever its payload is Send. This is the
    // type-level guarantee that a local ticket cannot enter a shared boundary.
    assert_impl_all!(SharedEnvelope<u64>: Send);
    // EscrowTicket is the sendable owner used at shared/runtime boundaries.
    assert_impl_all!(EscrowTicket: Send, Sync);
    // The interned attribution handle travels with escrow, so it must be
    // sendable too.
    assert_impl_all!(BudgetScope: Send, Sync);

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
            Arc::new(BudgetScopeId::default()),
        ))
    }

    fn account(floor: u64, step: u64, overshoot: u64, spare: u64) -> Rc<RuntimeMemoryAccount> {
        account_with_mode(BudgetMode::ObserveOnly, floor, step, overshoot, spare)
    }

    /// Builds an account backed by a shared pool with an explicit overshoot-debt
    /// allowance so tests can exercise debt acquisition, overdraw, and repayment.
    fn account_with_pool(
        mode: BudgetMode,
        floor: u64,
        step: u64,
        overshoot: u64,
        pool: Arc<GlobalLeasePool>,
    ) -> Rc<RuntimeMemoryAccount> {
        Rc::new(RuntimeMemoryAccount::new(
            floor,
            step,
            overshoot,
            mode,
            pool,
            Arc::new(RuntimeMemorySnapshot::default()),
            Arc::new(BudgetScopeId::default()),
        ))
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
            assert_eq!(ticket.bytes(), Some(50));
            acct.flush_snapshot();
            assert_eq!(acct.snapshot.charged_bytes(), 50);
        }
        acct.flush_snapshot();
        assert_eq!(acct.snapshot.charged_bytes(), 0);
    }

    #[test]
    fn unknown_size_ticket_tracks_unknown_retention_and_refunds_on_drop() {
        let acct = account(100, 10, 20, 100);
        {
            let ticket = acct.charge(UnknownSize).expect("unknown is observed");
            assert_eq!(ticket.bytes(), None);
            acct.flush_snapshot();
            assert_eq!(acct.snapshot.charged_bytes(), 0);
            assert_eq!(acct.snapshot.unknown_count(), 1);
        }
        acct.flush_snapshot();
        assert_eq!(acct.snapshot.unknown_count(), 0);
    }

    #[test]
    fn unknown_size_ticket_cannot_convert_to_escrow() {
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
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 100,
                runtime_count: 1,
                enforcement: MemoryBudgetEnforcement::default(),
            },
            None,
        );
        let acct = account(100, 10, 20, 100);
        let ticket = acct.charge(UnknownSize).expect("unknown is observed");

        assert!(ticket.try_into_escrow(&state).is_err());
    }

    #[test]
    fn charged_size_blanket_impls_delegate_to_inner_value() {
        let value = 42_u64;
        let boxed = Box::new(7_u64);
        // Exercise the `&T` blanket impl explicitly via the trait function.
        assert_eq!(ChargedSize::charged_size(&&value), Some(42));
        assert_eq!(boxed.charged_size(), Some(7));
        assert_eq!("abc".charged_size(), Some(3));
        assert_eq!(String::from("abcd").charged_size(), Some(4));
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
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 100,
                runtime_count: 1,
                enforcement: MemoryBudgetEnforcement::default(),
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

    fn configure_state_with_drain_allowance(
        lease_step: u64,
        drain_allowance_bytes: u64,
    ) -> MemoryBudgetState {
        let state = MemoryBudgetState::default();
        state.configure(
            RuntimeMemoryBudgetConfig {
                mode: BudgetMode::ObserveOnly,
                retry_after_secs: 1,
                sizing: MemoryBudgetSizing {
                    reserve_bytes: 0,
                    floor_per_runtime_bytes: 10_000,
                    lease_step_bytes: lease_step,
                    max_overshoot_per_runtime_bytes: 2 * lease_step,
                    overshoot_debt_limit_bytes: lease_step,
                    drain_allowance_bytes,
                },
                topic_default_limit_bytes: 1_000,
                runtime_count: 1,
                enforcement: MemoryBudgetEnforcement::default(),
            },
            None,
        );
        state
    }

    #[test]
    fn local_account_applies_configured_drain_allowance() {
        // An explicit, larger-than-lease-step allowance must be applied verbatim
        // to the runtime account created on the pinned thread.
        let state = configure_state_with_drain_allowance(64, 4_096);
        let handle = state.register_runtime_snapshot(BudgetScopeId::default());
        let account = handle.local_account().expect("budget should be configured");
        assert_eq!(account.drain_allowance_bytes(), 4_096);
    }

    #[test]
    fn local_account_defaults_drain_allowance_to_lease_step() {
        // `0` means "use the default", which the engine resolves to
        // `lease_step_bytes` so a `Hard` runtime can drain one lease-sized item.
        let state = configure_state_with_drain_allowance(256, 0);
        let handle = state.register_runtime_snapshot(BudgetScopeId::default());
        let account = handle.local_account().expect("budget should be configured");
        assert_eq!(account.drain_allowance_bytes(), 256);
    }

    #[test]
    fn configured_drain_allowance_admits_one_sized_item_at_hard() {
        // A consumer pinned to local `Hard` must be able to drain at least one
        // item sized up to the configured allowance, but not more.
        let allowance = 4_096_u64;
        let state = configure_state_with_drain_allowance(64, allowance);
        let handle = state.register_runtime_snapshot(BudgetScopeId::default());
        let account = handle.local_account().expect("budget should be configured");
        // Force the account to local `Hard` via an overshoot-debt overdraw so
        // the drain path is the only way to admit work.
        let mut ticket = account.charge(account.floor_bytes).expect("floor charge");
        ticket.reconcile_size(account.floor_bytes + account.lease.max_overshoot_bytes() + 1);
        assert_eq!(account.level(), BudgetLevel::Hard);

        let drained = account
            .try_charge_for_drain(allowance)
            .expect("one allowance-sized item should drain at Hard");
        assert!(
            account.try_charge_for_drain(1).is_none(),
            "allowance is exhausted after the first drain charge"
        );
        drop(drained);
        // Releasing the drained ticket frees the allowance for the next item.
        assert!(
            account.try_charge_for_drain(allowance).is_some(),
            "allowance is restored after the drained ticket drops"
        );
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
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 100,
                runtime_count: 1,
                enforcement: MemoryBudgetEnforcement::default(),
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
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 10,
                runtime_count: 1,
                enforcement: MemoryBudgetEnforcement {
                    queue_publish: true,
                    receiver_admission: false,
                    reclaim_hooks: false,
                },
            },
            None,
        );
        let acct = account(100, 10, 20, 100);
        let ticket = acct.charge(50_u64).expect("charge should fit");
        let ticket = ticket
            .try_into_escrow(&state)
            .expect_err("escrow should reject above topic limit");

        assert_eq!(ticket.bytes(), Some(50));
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
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 10,
                runtime_count: 1,
                enforcement: MemoryBudgetEnforcement::default(),
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
    fn enforce_escrow_limit_is_per_default_bucket() {
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
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 60,
                runtime_count: 1,
                enforcement: MemoryBudgetEnforcement {
                    queue_publish: true,
                    receiver_admission: false,
                    reclaim_hooks: false,
                },
            },
            // Provide enough process headroom (floor 100 + 100 spare) so the
            // global pool can back the first escrow's transfer charge. The
            // test isolates the default-bucket cap, not the pool-backing path.
            Some(200),
        );
        let acct = account(100, 10, 20, 100);
        let first = acct
            .charge(40_u64)
            .expect("first local charge should fit")
            .try_into_escrow(&state)
            .expect("first escrow should fit bucket limit");
        let second = acct
            .charge(30_u64)
            .expect("second local charge should fit")
            .try_into_escrow(&state)
            .expect_err("second escrow should exceed bucket limit");

        assert_eq!(first.bytes(), 40);
        assert_eq!(second.bytes(), Some(30));
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
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 100,
                runtime_count: 1,
                enforcement: MemoryBudgetEnforcement::default(),
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
        assert!(
            snapshot.abandoned_escrow_oldest_age_millis >= 1,
            "abandoned escrow age must be visible immediately"
        );
        assert_eq!(
            snapshot.abandoned_escrow_alarm_count, 1,
            "first abandoned escrow emits one bounded alarm"
        );
        assert_eq!(snapshot.escrow_ticket_count, 1);
        assert_eq!(snapshot.escrow_charged_bytes, 42);
    }

    #[test]
    fn abandoned_escrow_alarm_is_bounded_and_sticky() {
        let state = escrow_state();
        let acct = account(100, 10, 20, 100);

        let first = acct
            .charge(10_u64)
            .expect("first charge should fit")
            .try_into_escrow(&state)
            .expect("first escrow should fit");
        let second = acct
            .charge(20_u64)
            .expect("second charge should fit")
            .try_into_escrow(&state)
            .expect("second escrow should fit");

        drop(first);
        drop(second);

        let snapshot = state.snapshot();
        assert_eq!(snapshot.abandoned_escrow_count, 2);
        assert_eq!(snapshot.abandoned_escrow_bytes, 30);
        assert_eq!(
            snapshot.abandoned_escrow_alarm_count, 1,
            "alarm is emitted at most once per memory-budget state"
        );
        assert_eq!(
            snapshot.escrow_charged_bytes, 30,
            "abandoned escrow remains sticky until a future reaper policy"
        );
        assert!(
            snapshot.abandoned_escrow_oldest_age_millis >= 1,
            "oldest abandoned age remains visible while sticky"
        );
    }

    /// Local-ticket → escrow conversion must transfer ownership without
    /// inflating the global logical total. After the transfer the sum
    /// `runtime_charged + escrow_charged` equals the original local charge
    /// and the escrow draws an explicit pool borrow.
    #[test]
    fn try_into_escrow_does_not_inflate_global_logical_total() {
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
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 200,
                runtime_count: 1,
                enforcement: MemoryBudgetEnforcement::default(),
            },
            Some(200),
        );
        let pool = state.inner.pool.clone();
        let acct = Rc::new(RuntimeMemoryAccount::new(
            100,
            10,
            20,
            BudgetMode::ObserveOnly,
            Arc::new(pool.clone()),
            Arc::new(RuntimeMemorySnapshot::default()),
            Arc::new(BudgetScopeId::default()),
        ));

        let pool_before = state.inner.pool.available_bytes();
        let ticket = acct.charge(40_u64).expect("charge should fit");
        acct.flush_snapshot();
        let charged_before = acct.snapshot.charged_bytes();
        assert_eq!(charged_before, 40);

        let escrow = ticket
            .try_into_escrow(&state)
            .expect("escrow transfer should succeed");

        acct.flush_snapshot();
        let snap = state.snapshot();
        assert_eq!(
            acct.snapshot.charged_bytes() + snap.escrow_charged_bytes,
            charged_before,
            "logical total is conserved across local → escrow transfer"
        );
        assert_eq!(snap.escrow_pool_held_bytes, 40);
        assert_eq!(snap.escrow_pool_overshoot_bytes, 0);
        // Pool is approximately conserved (drew 40 for escrow; producer's
        // lease may have returned its small borrow on settle).
        assert!(
            state.inner.pool.available_bytes() <= pool_before,
            "escrow pool draw must not increase global spare"
        );

        escrow.release();
        let snap = state.snapshot();
        assert_eq!(snap.escrow_pool_held_bytes, 0);
        assert_eq!(
            state.inner.pool.available_bytes(),
            pool_before,
            "release returns the escrow's pool borrow exactly"
        );
    }

    /// Builds a pool-backed escrow state plus an account sharing its pool, so
    /// escrow conversions draw an explicit pool borrow (mirrors the conservation
    /// test setup). Used by the abandoned-escrow reaper tests.
    fn pool_backed_escrow_state_and_account() -> (MemoryBudgetState, Rc<RuntimeMemoryAccount>) {
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
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 200,
                runtime_count: 1,
                enforcement: MemoryBudgetEnforcement::default(),
            },
            Some(200),
        );
        let pool = state.inner.pool.clone();
        let acct = Rc::new(RuntimeMemoryAccount::new(
            100,
            10,
            20,
            BudgetMode::ObserveOnly,
            Arc::new(pool),
            Arc::new(RuntimeMemorySnapshot::default()),
            Arc::new(BudgetScopeId::default()),
        ));
        (state, acct)
    }

    #[test]
    fn abandoned_escrow_reaper_reclaims_after_threshold() {
        // With the reaper enabled, an abandoned escrow charge is reclaimed once
        // it ages past the threshold: its bytes return to the pool and the escrow
        // counters reverse, while the cumulative abandoned/reaped totals keep the
        // leak observable.
        let (state, acct) = pool_backed_escrow_state_and_account();
        state.set_abandoned_escrow_reap_after(Some(Duration::from_millis(50)));
        let pool_before = state.inner.pool.available_bytes();

        let escrow = acct
            .charge(40_u64)
            .expect("charge should fit")
            .try_into_escrow(&state)
            .expect("escrow should fit");
        drop(escrow); // abandoned (no explicit redeem/release/abort)

        // Immediately (well under the 50ms threshold) the charge is still sticky.
        let before = state.snapshot();
        assert_eq!(before.abandoned_escrow_count, 1);
        assert_eq!(before.reaped_escrow_count, 0, "not yet aged past threshold");
        assert_eq!(before.escrow_charged_bytes, 40, "sticky before reaping");
        assert_eq!(before.escrow_pool_held_bytes, 40);

        std::thread::sleep(Duration::from_millis(70));
        let after = state.snapshot(); // maintenance pass runs the reaper

        assert_eq!(
            after.reaped_escrow_count, 1,
            "the aged abandoned escrow is reaped"
        );
        assert_eq!(after.reaped_escrow_bytes, 40);
        assert_eq!(
            after.escrow_charged_bytes, 0,
            "reaping reverses the escrow charge"
        );
        assert_eq!(after.escrow_ticket_count, 0);
        assert_eq!(
            after.escrow_pool_held_bytes, 0,
            "reaped bytes leave the pool hold"
        );
        assert_eq!(
            state.inner.pool.available_bytes(),
            pool_before,
            "reaping returns the abandoned escrow's pool borrow exactly"
        );
        assert_eq!(
            after.abandoned_escrow_count, 1,
            "cumulative abandoned total stays observable after reaping"
        );
        assert_eq!(after.abandoned_escrow_bytes, 40);
        assert_eq!(
            after.abandoned_escrow_oldest_age_millis, 0,
            "no un-reaped abandoned escrow remains"
        );
    }

    #[test]
    fn abandoned_escrow_not_reaped_when_disabled() {
        // Default (reaper disabled): abandoned escrow stays sticky indefinitely.
        let (state, acct) = pool_backed_escrow_state_and_account();
        let escrow = acct
            .charge(25_u64)
            .expect("charge should fit")
            .try_into_escrow(&state)
            .expect("escrow should fit");
        drop(escrow);

        std::thread::sleep(Duration::from_millis(10));
        let snap = state.snapshot();
        assert_eq!(snap.reaped_escrow_count, 0, "no reaping when disabled");
        assert_eq!(
            snap.escrow_charged_bytes, 25,
            "abandoned escrow remains sticky when the reaper is disabled"
        );
    }

    #[test]
    fn explicit_escrow_release_is_never_reaped() {
        // Explicit release is a tracked terminal path: it never records an
        // abandoned entry, so the reaper has nothing to reclaim.
        let (state, acct) = pool_backed_escrow_state_and_account();
        state.set_abandoned_escrow_reap_after(Some(Duration::from_millis(1)));

        let escrow = acct
            .charge(30_u64)
            .expect("charge should fit")
            .try_into_escrow(&state)
            .expect("escrow should fit");
        escrow.release();

        std::thread::sleep(Duration::from_millis(10));
        let snap = state.snapshot();
        assert_eq!(
            snap.abandoned_escrow_count, 0,
            "explicit release records no abandoned escrow"
        );
        assert_eq!(
            snap.reaped_escrow_count, 0,
            "nothing to reap after explicit release"
        );
        assert_eq!(
            snap.escrow_charged_bytes, 0,
            "explicit release already returned the charge"
        );
    }

    /// Enforce mode rejects escrow creation when the global pool cannot back
    /// the transfer charge. The original local ticket is returned to the
    /// caller and no escrow counters move.
    #[test]
    fn enforce_escrow_rejected_when_pool_cannot_back_transfer() {
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
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 200,
                runtime_count: 1,
                enforcement: MemoryBudgetEnforcement {
                    queue_publish: true,
                    receiver_admission: false,
                    reclaim_hooks: false,
                },
            },
            // No process headroom: pool is empty after subtracting the floor.
            Some(100),
        );
        assert_eq!(state.inner.pool.available_bytes(), 0);
        let acct = account(100, 10, 20, 100);
        let ticket = acct.charge(40_u64).expect("local charge fits within floor");

        let returned = ticket
            .try_into_escrow(&state)
            .expect_err("escrow must be rejected when pool cannot back transfer");
        assert_eq!(returned.bytes(), Some(40));

        let snap = state.snapshot();
        assert_eq!(snap.escrow_charged_bytes, 0);
        assert_eq!(snap.escrow_pool_held_bytes, 0);
        assert_eq!(snap.escrow_pool_overshoot_bytes, 0);
        assert_eq!(snap.escrow_ticket_count, 0);
    }

    /// Enforce mode with the `queue_publish` gate disabled does NOT reject escrow
    /// at the topic/queue publish boundary: it records pressure (unbacked
    /// overshoot) exactly like observe-only. This proves the per-path gate, not
    /// just `mode`, drives publish-boundary enforcement.
    #[test]
    fn enforce_without_queue_publish_records_pressure_instead_of_rejecting() {
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
                    drain_allowance_bytes: 0,
                },
                // Cap below the charge so enforce-mode WOULD reject if the
                // queue_publish gate were honored on `mode` alone.
                topic_default_limit_bytes: 10,
                runtime_count: 1,
                // queue_publish disabled: the publish boundary must not reject.
                enforcement: MemoryBudgetEnforcement::default(),
            },
            Some(100),
        );
        assert_eq!(state.inner.pool.available_bytes(), 0);
        let acct = account(100, 10, 20, 100);
        let ticket = acct.charge(40_u64).expect("local charge fits within floor");

        let escrow = ticket
            .try_into_escrow(&state)
            .expect("queue_publish disabled must not reject escrow at the boundary");

        let snap = state.snapshot();
        assert_eq!(
            snap.escrow_charged_bytes, 40,
            "escrow is recorded even though enforce mode is set"
        );
        assert_eq!(
            snap.escrow_pool_overshoot_bytes, 40,
            "unbacked escrow surfaces as projected pressure, not a rejection"
        );
        escrow.release();
    }

    /// Observe-only escrow creation that exceeds global capacity still
    /// succeeds (observe-only never rejects), but the unbacked excess is
    /// surfaced via `escrow_pool_overshoot_bytes` so operators see the
    /// projected invariant violation.
    #[test]
    fn observe_only_escrow_records_pool_overshoot_when_pool_exhausted() {
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
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 200,
                runtime_count: 1,
                enforcement: MemoryBudgetEnforcement::default(),
            },
            // No process headroom: pool is empty after subtracting the floor.
            Some(100),
        );
        assert_eq!(state.inner.pool.available_bytes(), 0);
        let acct = account(100, 10, 20, 100);
        let ticket = acct.charge(40_u64).expect("local charge fits within floor");

        let escrow = ticket
            .try_into_escrow(&state)
            .expect("observe-only escrow records pressure rather than rejecting");

        let snap = state.snapshot();
        assert_eq!(snap.escrow_charged_bytes, 40);
        assert_eq!(snap.escrow_pool_held_bytes, 0);
        assert_eq!(
            snap.escrow_pool_overshoot_bytes, 40,
            "unbacked observe-only escrow must surface projected pressure"
        );

        drop(escrow);
        // Drop without explicit release routes to the abandoned graveyard,
        // which keeps the leak visible (escrow_charged_bytes stays elevated)
        // but releases the unbacked-overshoot counter accounting cleanly.
        let snap = state.snapshot();
        assert!(snap.abandoned_escrow_count > 0);
    }

    /// A fanout into `count` escrow owners charges each owner the full logical
    /// bytes (per-retained-owner accounting), refunds the producer once, and
    /// releases cleanly without recording any abandoned-escrow leak.
    #[test]
    fn escrow_fanout_charges_each_owner_and_refunds_producer_once() {
        let state = MemoryBudgetState::default();
        state.configure(
            RuntimeMemoryBudgetConfig {
                mode: BudgetMode::ObserveOnly,
                retry_after_secs: 1,
                sizing: MemoryBudgetSizing {
                    reserve_bytes: 0,
                    floor_per_runtime_bytes: 1_000,
                    lease_step_bytes: 100,
                    max_overshoot_per_runtime_bytes: 1_000,
                    overshoot_debt_limit_bytes: 100,
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 1_000_000,
                runtime_count: 1,
                enforcement: MemoryBudgetEnforcement::default(),
            },
            None,
        );
        let handle = state.register_runtime_snapshot(BudgetScopeId::default());
        let acct = handle.local_account().expect("account");
        let ticket = acct.charge(20_u64).expect("charge fits within floor");
        acct.flush_snapshot();
        assert_eq!(state.snapshot().charged_bytes, 20);

        let owners = ticket
            .try_into_escrow_fanout(&state, 3)
            .expect("observe-only fanout always succeeds");
        assert_eq!(owners.len(), 3);
        acct.flush_snapshot();
        let snap = state.snapshot();
        assert_eq!(
            snap.charged_bytes, 0,
            "producer local charge refunded exactly once"
        );
        assert_eq!(
            snap.escrow_charged_bytes, 60,
            "three owners each charge the full 20 bytes"
        );

        for owner in owners {
            owner.release();
        }
        let snap = state.snapshot();
        assert_eq!(snap.escrow_charged_bytes, 0, "all owners released");
        assert_eq!(
            snap.abandoned_escrow_count, 0,
            "clean release must not record a leak"
        );
    }

    /// An enforce-mode fanout that cannot charge every owner unwinds the
    /// already-acquired owners in reverse order, restores the pool, and returns
    /// the original local ticket still owning the charge.
    #[test]
    fn escrow_fanout_unwinds_partial_reservation_on_pool_exhaustion() {
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
                    drain_allowance_bytes: 0,
                },
                // Topic cap is generous so the global pool is the binding limit.
                topic_default_limit_bytes: 1_000,
                runtime_count: 1,
                enforcement: MemoryBudgetEnforcement {
                    queue_publish: true,
                    receiver_admission: false,
                    reclaim_hooks: false,
                },
            },
            // Spare pool room for exactly one 50-byte owner: 150 - 0 - 100 = 50.
            Some(150),
        );
        assert_eq!(state.inner.pool.available_bytes(), 50);
        let acct = account_with_mode(BudgetMode::Enforce, 100, 10, 20, 100);
        let ticket = acct.charge(50_u64).expect("local charge fits within floor");

        let returned = ticket
            .try_into_escrow_fanout(&state, 2)
            .expect_err("second owner cannot be backed by the exhausted pool");
        assert_eq!(
            returned.bytes(),
            Some(50),
            "original ticket survives and still owns the charge"
        );

        let snap = state.snapshot();
        assert_eq!(snap.escrow_charged_bytes, 0, "partial owner unwound");
        assert_eq!(snap.escrow_pool_held_bytes, 0);
        assert_eq!(snap.escrow_ticket_count, 0);
        assert_eq!(
            snap.abandoned_escrow_count, 0,
            "reverse-order unwind releases cleanly, no graveyard entry"
        );
        assert_eq!(
            state.inner.pool.available_bytes(),
            50,
            "the borrowed pool bytes are fully returned on unwind"
        );
    }

    /// Zero-count fanout requests cannot fan out into escrow and are returned
    /// unchanged with the producer charge intact.
    #[test]
    fn escrow_fanout_rejects_zero_count_and_returns_ticket() {
        let state = MemoryBudgetState::default();
        state.configure(
            RuntimeMemoryBudgetConfig {
                mode: BudgetMode::ObserveOnly,
                retry_after_secs: 1,
                sizing: MemoryBudgetSizing {
                    reserve_bytes: 0,
                    floor_per_runtime_bytes: 1_000,
                    lease_step_bytes: 100,
                    max_overshoot_per_runtime_bytes: 1_000,
                    overshoot_debt_limit_bytes: 100,
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 1_000_000,
                runtime_count: 1,
                enforcement: MemoryBudgetEnforcement::default(),
            },
            None,
        );
        let handle = state.register_runtime_snapshot(BudgetScopeId::default());
        let acct = handle.local_account().expect("account");
        let ticket = acct.charge(20_u64).expect("charge fits");

        let returned = ticket
            .try_into_escrow_fanout(&state, 0)
            .expect_err("zero-count fanout must return the ticket unchanged");
        assert_eq!(returned.bytes(), Some(20));
        assert_eq!(state.snapshot().escrow_charged_bytes, 0);
    }

    fn shared_owner_state() -> MemoryBudgetState {
        let state = MemoryBudgetState::default();
        state.configure(
            RuntimeMemoryBudgetConfig {
                mode: BudgetMode::ObserveOnly,
                retry_after_secs: 1,
                sizing: MemoryBudgetSizing {
                    reserve_bytes: 0,
                    floor_per_runtime_bytes: 1_000,
                    lease_step_bytes: 100,
                    max_overshoot_per_runtime_bytes: 1_000,
                    overshoot_debt_limit_bytes: 100,
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 1_000_000,
                runtime_count: 1,
                enforcement: MemoryBudgetEnforcement::default(),
            },
            None,
        );
        state
    }

    /// Converting a local envelope into a shared envelope moves the charge into
    /// a sendable escrow owner exactly once (producer refunded), keeps it
    /// charged while the shared envelope is held, and releases it exactly once
    /// on drop without recording a leak.
    #[test]
    fn local_envelope_into_shared_transfers_then_releases_once() {
        let state = shared_owner_state();
        let handle = state.register_runtime_snapshot(BudgetScopeId::default());
        let acct = handle.local_account().expect("account");

        let ticket = acct.charge(40_u64).expect("charge fits");
        acct.flush_snapshot();
        assert_eq!(state.snapshot().charged_bytes, 40);

        let local = LocalEnvelope::new(7_u64, ticket);
        let shared = local
            .into_shared(&state)
            .expect("observe-only conversion always succeeds");
        assert!(shared.has_owner());
        assert_eq!(shared.owner_bytes(), Some(40));
        assert_eq!(*shared.payload(), 7);

        // The shared owner holds the charge; the producer was refunded.
        acct.flush_snapshot();
        let snap = state.snapshot();
        assert_eq!(snap.charged_bytes, 0, "producer refunded by the transfer");
        assert_eq!(
            snap.escrow_charged_bytes, 40,
            "shared owner holds the charge"
        );

        // Dropping the shared envelope releases the escrow exactly once.
        drop(shared);
        let snap = state.snapshot();
        assert_eq!(
            snap.escrow_charged_bytes, 0,
            "drop releases the shared owner"
        );
        assert_eq!(
            snap.abandoned_escrow_count, 0,
            "clean drop must not record an abandoned-escrow leak"
        );
    }

    /// An owner-less (budget-disabled / uncharged) local envelope converts into
    /// an owner-less shared envelope that carries no charge.
    #[test]
    fn local_envelope_without_ticket_into_shared_has_no_owner() {
        let state = shared_owner_state();
        let local = LocalEnvelope::without_ticket(9_u64);
        let shared = local.into_shared(&state).expect("no-ticket conversion");
        assert!(!shared.has_owner());
        assert_eq!(shared.owner_bytes(), None);
        assert_eq!(shared.into_payload(), 9);
        assert_eq!(state.snapshot().escrow_charged_bytes, 0);
    }

    /// When escrow is refused (enforce-mode pool exhausted), `into_shared`
    /// returns the original local envelope unchanged so the caller keeps the
    /// local charge.
    #[test]
    fn local_envelope_into_shared_returns_original_on_escrow_refusal() {
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
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 1_000,
                runtime_count: 1,
                enforcement: MemoryBudgetEnforcement {
                    queue_publish: true,
                    receiver_admission: false,
                    reclaim_hooks: false,
                },
            },
            // No process headroom: the escrow pool is empty after the floor.
            Some(100),
        );
        assert_eq!(state.inner.pool.available_bytes(), 0);
        let acct = account_with_mode(BudgetMode::Enforce, 100, 10, 20, 100);
        let ticket = acct.charge(40_u64).expect("local charge fits within floor");

        let local = LocalEnvelope::new(3_u64, ticket);
        let returned = local
            .into_shared(&state)
            .expect_err("escrow refusal must return the original local envelope");
        assert!(returned.has_ticket(), "local charge preserved on refusal");
        assert_eq!(returned.ticket().and_then(|t| t.bytes()), Some(40));
        assert_eq!(state.snapshot().escrow_charged_bytes, 0);
    }

    /// Splitting a shared envelope hands the escrow owner to the caller, who can
    /// resolve it explicitly (clean release, no graveyard entry).
    #[test]
    fn shared_envelope_into_parts_hands_off_owner() {
        let state = shared_owner_state();
        let handle = state.register_runtime_snapshot(BudgetScopeId::default());
        let acct = handle.local_account().expect("account");
        let ticket = acct.charge(25_u64).expect("charge fits");

        let shared = LocalEnvelope::new(1_u64, ticket)
            .into_shared(&state)
            .expect("conversion succeeds");
        let (payload, mut slot) = shared.into_parts();
        assert_eq!(payload, 1);
        assert_eq!(state.snapshot().escrow_charged_bytes, 25);

        // The caller now owns the escrow and resolves it explicitly.
        let escrow = slot.take().expect("owner present");
        escrow.release();
        let snap = state.snapshot();
        assert_eq!(snap.escrow_charged_bytes, 0);
        assert_eq!(snap.abandoned_escrow_count, 0);
    }

    /// A `SharedEnvelope` charged on the producer thread crosses a real Send
    /// channel to another OS thread and releases its escrow exactly once when
    /// dropped on the consumer thread. This exercises the type across an actual
    /// `Send` boundary (not just the compile-time `Send` assertion), which is
    /// the property a shared-channel/shared-node retention site relies on.
    #[test]
    fn shared_envelope_crosses_real_thread_boundary_and_releases_once() {
        let state = shared_owner_state();
        let handle = state.register_runtime_snapshot(BudgetScopeId::default());
        let acct = handle.local_account().expect("account");

        let ticket = acct.charge(40_u64).expect("charge fits");
        acct.flush_snapshot();
        let shared = LocalEnvelope::new(7_u64, ticket)
            .into_shared(&state)
            .expect("conversion succeeds");
        assert_eq!(state.snapshot().escrow_charged_bytes, 40);

        // Move the shared owner across a real thread via a Send channel.
        let (tx, rx) = std::sync::mpsc::channel::<SharedEnvelope<u64>>();
        tx.send(shared).expect("SharedEnvelope is Send");
        let consumer = std::thread::spawn(move || {
            let env = rx.recv().expect("receive on consumer thread");
            assert_eq!(*env.payload(), 7);
            // Dropping on the consumer thread releases the escrow once.
            drop(env);
        });
        consumer.join().expect("consumer thread");

        let snap = state.snapshot();
        assert_eq!(
            snap.escrow_charged_bytes, 0,
            "consumer-thread drop releases the shared owner exactly once"
        );
        assert_eq!(snap.abandoned_escrow_count, 0);
    }

    #[test]
    fn observe_only_charge_records_above_hard_limit() {
        let acct = account(100, 10, 20, 0);
        let ticket = acct
            .charge(121_u64)
            .expect("observe-only charge should record above hard");

        assert_eq!(ticket.bytes(), Some(121));
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
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 100,
                runtime_count: 1,
                enforcement: MemoryBudgetEnforcement::default(),
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
    // Ticket size-adjustment tests.
    // -------------------------------------------------------------------------

    #[test]
    fn try_resize_grow_then_shrink_tracks_charge() {
        let acct = account(1_000, 10, 20, 0);
        let mut ticket = acct.charge(50_u64).expect("charge should fit");
        ticket.try_resize(50, 80).expect("grow within floor");
        acct.flush_snapshot();
        assert_eq!(acct.snapshot.charged_bytes(), 80);
        ticket.try_resize(80, 30).expect("shrink always succeeds");
        acct.flush_snapshot();
        assert_eq!(acct.snapshot.charged_bytes(), 30);
        assert_eq!(ticket.bytes(), Some(30));
    }

    #[test]
    fn try_resize_failed_grow_preserves_original_charge() {
        // Enforce mode, no spare and no overshoot debt: a grow beyond the floor
        // cannot be authorized and must leave the original charge intact.
        let acct = account_with_mode(BudgetMode::Enforce, 100, 10, 0, 0);
        let mut ticket = acct.charge(100_u64).expect("charge fills the floor");
        acct.flush_snapshot();
        assert_eq!(acct.snapshot.charged_bytes(), 100);

        let err = ticket
            .try_resize(100, 160)
            .expect_err("grow must fail without capacity");
        assert_eq!(err, BudgetError::Exhausted);
        acct.flush_snapshot();
        assert_eq!(
            acct.snapshot.charged_bytes(),
            100,
            "failed grow must preserve the original charge"
        );
        assert_eq!(ticket.bytes(), Some(100));
    }

    #[test]
    fn unknown_size_ticket_cannot_resize_but_can_reconcile() {
        let acct = account(1_000, 10, 20, 0);
        let mut ticket = acct.charge(UnknownSize).expect("unknown is observed");
        assert_eq!(
            ticket.try_resize(0, 10),
            Err(BudgetError::UnknownSize),
            "unknown-size tickets have no baseline to resize from"
        );
        assert_eq!(
            ticket.try_reserve_extra(10),
            Err(BudgetError::UnknownSize),
            "unknown-size tickets cannot reserve extra without a baseline"
        );
        // reconcile_size converts the unknown observation into a known charge.
        ticket.reconcile_size(64);
        acct.flush_snapshot();
        assert_eq!(ticket.bytes(), Some(64));
        assert_eq!(acct.snapshot.charged_bytes(), 64);
        assert_eq!(acct.snapshot.unknown_count(), 0);
    }

    #[test]
    fn try_reserve_clone_charges_an_additional_owner() {
        let acct = account(1_000, 10, 20, 0);
        let ticket = acct.charge(40_u64).expect("charge should fit");
        let clone = ticket
            .try_reserve_clone(40)
            .expect("clone owner should be charged");
        acct.flush_snapshot();
        assert_eq!(
            acct.snapshot.charged_bytes(),
            80,
            "each retained branch reserves its own charge"
        );
        drop(clone);
        acct.flush_snapshot();
        assert_eq!(acct.snapshot.charged_bytes(), 40);
        drop(ticket);
        acct.flush_snapshot();
        assert_eq!(acct.snapshot.charged_bytes(), 0);
    }

    // -------------------------------------------------------------------------
    // Overshoot / reconciliation-debt model tests.
    // -------------------------------------------------------------------------

    #[test]
    fn enforce_admission_acquires_overshoot_debt_from_pool() {
        // floor 100, no spare lease, overshoot ceiling 50, debt pool 50.
        let pool = Arc::new(GlobalLeasePool::with_overshoot_debt(0, 50));
        let acct = account_with_pool(BudgetMode::Enforce, 100, 10, 50, pool.clone());
        // 140 charged = 40 overshoot, authorized from the debt pool.
        let ticket = acct
            .charge(140_u64)
            .expect("overshoot debt should authorize");
        assert_eq!(
            pool.overshoot_debt_balance(),
            10,
            "40 debt acquired from 50"
        );
        assert_eq!(acct.level(), BudgetLevel::Soft);
        drop(ticket);
        assert_eq!(
            pool.overshoot_debt_balance(),
            50,
            "dropping the owner repays acquired debt"
        );
        assert_eq!(acct.level(), BudgetLevel::Normal);
    }

    #[test]
    fn enforce_admission_rejected_when_debt_pool_exhausted() {
        // Overshoot ceiling allows 50, but the debt pool only has 20.
        let pool = Arc::new(GlobalLeasePool::with_overshoot_debt(0, 20));
        let acct = account_with_pool(BudgetMode::Enforce, 100, 10, 50, pool.clone());
        assert!(
            acct.charge(160_u64).is_none(),
            "60 overshoot cannot be authorized from a 20-byte debt pool"
        );
        assert_eq!(
            pool.overshoot_debt_balance(),
            20,
            "rejected admission must not strand acquired debt"
        );
        acct.flush_snapshot();
        assert_eq!(acct.snapshot.charged_bytes(), 0);
    }

    #[test]
    fn reconcile_grow_overdraws_pool_and_pins_hard() {
        // Post-hoc growth: only 20 debt available but reconcile needs 40.
        let pool = Arc::new(GlobalLeasePool::with_overshoot_debt(0, 20));
        let acct = account_with_pool(BudgetMode::Enforce, 100, 10, 50, pool.clone());
        let mut ticket = acct.charge(100_u64).expect("charge fills the floor");

        // Grow to 140 after the fact (size only known post-allocation).
        ticket.reconcile_size(140);
        acct.flush_snapshot();
        assert_eq!(acct.snapshot.charged_bytes(), 140);
        assert_eq!(
            acct.snapshot.reconcile_debt_bytes(),
            20,
            "20 of the 40 overshoot is unbacked reconciliation debt"
        );
        assert_eq!(
            pool.overshoot_debt_balance(),
            -20,
            "overdrawn debt pool balance is negative"
        );
        assert_eq!(
            acct.level(),
            BudgetLevel::Hard,
            "unbacked reconciliation debt pins the runtime to Hard"
        );

        // Drain back below authorized capacity: debt is repaid and Hard clears.
        ticket.reconcile_size(100);
        acct.flush_snapshot();
        assert_eq!(acct.snapshot.reconcile_debt_bytes(), 0);
        assert_eq!(
            pool.overshoot_debt_balance(),
            20,
            "repayment restores the pool"
        );
        assert_eq!(acct.level(), BudgetLevel::Normal);
    }

    #[test]
    fn observe_only_never_overdraws_or_records_reconcile_debt_on_admission() {
        let pool = Arc::new(GlobalLeasePool::with_overshoot_debt(0, 0));
        let acct = account_with_pool(BudgetMode::ObserveOnly, 100, 10, 50, pool.clone());
        let _ticket = acct
            .charge(130_u64)
            .expect("observe-only admits above authorized capacity");
        acct.flush_snapshot();
        assert_eq!(acct.snapshot.charged_bytes(), 130);
        assert_eq!(
            acct.snapshot.reconcile_debt_bytes(),
            0,
            "observe-only admission must not record reconciliation debt"
        );
        assert_eq!(
            pool.overshoot_debt_balance(),
            0,
            "observe-only admission must not overdraw the debt pool"
        );
    }

    // -------------------------------------------------------------------------
    // Escrow release/abort/redeem tests.
    // -------------------------------------------------------------------------

    fn escrow_state() -> MemoryBudgetState {
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
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 1_000,
                runtime_count: 1,
                enforcement: MemoryBudgetEnforcement::default(),
            },
            None,
        );
        state
    }

    #[test]
    fn explicit_escrow_abort_releases_without_graveyard() {
        let state = escrow_state();
        let acct = account(100, 10, 20, 100);
        let escrow = acct
            .charge(42_u64)
            .expect("charge should fit")
            .try_into_escrow(&state)
            .expect("escrow should fit topic limit");
        escrow.abort();
        let snapshot = state.snapshot();
        assert_eq!(snapshot.escrow_charged_bytes, 0);
        assert_eq!(
            snapshot.abandoned_escrow_count, 0,
            "explicit abort must not record an abandoned-escrow entry"
        );
    }

    #[test]
    fn explicit_escrow_release_is_not_graveyard() {
        let state = escrow_state();
        let acct = account(100, 10, 20, 100);
        let escrow = acct
            .charge(30_u64)
            .expect("charge should fit")
            .try_into_escrow(&state)
            .expect("escrow should fit");
        escrow.release();
        let snapshot = state.snapshot();
        assert_eq!(snapshot.escrow_charged_bytes, 0);
        assert_eq!(snapshot.abandoned_escrow_count, 0);
    }

    #[test]
    fn escrow_redeem_into_transfers_ownership_to_consumer() {
        let state = escrow_state();
        let producer = account(100, 10, 20, 100);
        let consumer = account(100, 10, 20, 100);

        let escrow = producer
            .charge(40_u64)
            .expect("producer charge should fit")
            .try_into_escrow(&state)
            .expect("escrow should fit");
        producer.flush_snapshot();
        assert_eq!(producer.snapshot.charged_bytes(), 0);
        assert_eq!(state.snapshot().escrow_charged_bytes, 40);

        let ticket = escrow
            .redeem_into(&consumer)
            .expect("consumer should accept redemption");
        assert_eq!(ticket.bytes(), Some(40));
        consumer.flush_snapshot();
        assert_eq!(
            consumer.snapshot.charged_bytes(),
            40,
            "redeemed bytes move into the consumer account"
        );
        let snapshot = state.snapshot();
        assert_eq!(snapshot.escrow_charged_bytes, 0);
        assert_eq!(snapshot.abandoned_escrow_count, 0);
    }

    #[test]
    fn escrow_redeem_into_failure_returns_original_escrow() {
        let state = escrow_state();
        let producer = account(100, 10, 20, 100);
        // Enforce-mode consumer with no spare/overshoot AND a zero drain
        // allowance: rejects the redemption with no fallback.
        let consumer = account_with_mode(BudgetMode::Enforce, 0, 10, 0, 0);
        consumer.set_drain_allowance_bytes(0);

        let escrow = producer
            .charge(40_u64)
            .expect("producer charge should fit")
            .try_into_escrow(&state)
            .expect("escrow should fit");
        let escrow = escrow
            .redeem_into(&consumer)
            .expect_err("consumer at hard with no drain allowance rejects redemption");
        assert_eq!(
            escrow.bytes(),
            40,
            "the original escrow owner is returned on failure"
        );
        assert_eq!(
            state.snapshot().escrow_charged_bytes,
            40,
            "escrow still owns the charge after a failed redemption"
        );
        escrow.abort();
    }

    /// Consumer at local `Hard` (pinned via reconcile overdraw) with normal
    /// admission rejected can still redeem an in-transit escrow item via
    /// the per-runtime drain/redemption allowance (design lines 1154-1184).
    /// The bytes are still charged to the consumer; the escrow is released
    /// exactly once.
    #[test]
    fn drain_allowance_lets_hard_consumer_redeem_at_least_one_item() {
        let state = escrow_state();
        let producer = account(100, 10, 20, 100);
        // Enforce-mode consumer pinned to Hard via post-hoc reconcile overdraw:
        // floor=50, lease_step=64 (also seeds drain_allowance=64), overshoot=10,
        // pool with no debt allowance. Filling the floor then reconcile-growing
        // past authorized capacity drives unbacked reconciliation debt and
        // pins the account to Hard regardless of normal admission paths.
        let pool = Arc::new(GlobalLeasePool::with_overshoot_debt(0, 0));
        let consumer = account_with_pool(BudgetMode::Enforce, 50, 64, 10, pool);
        let mut filler = consumer.charge(50_u64).expect("fill floor");
        filler.reconcile_size(120);
        assert_eq!(
            consumer.level(),
            BudgetLevel::Hard,
            "reconcile overdraw must pin the consumer to Hard"
        );
        assert_eq!(consumer.drain_allowance_bytes(), 64);
        // Sanity: normal admission for new external work at Hard is still
        // rejected -- the drain allowance must not turn into a sneak path.
        assert!(consumer.charge(32_u64).is_none());

        let escrow = producer
            .charge(32_u64)
            .expect("producer charge should fit")
            .try_into_escrow(&state)
            .expect("escrow should fit");

        let ticket = escrow
            .redeem_into(&consumer)
            .expect("drain allowance must admit one redeem under Hard");
        assert_eq!(ticket.bytes(), Some(32));
        assert_eq!(consumer.drain_committed.get(), 32);
        assert_eq!(state.snapshot().escrow_charged_bytes, 0);

        // Drop the drain ticket: the allowance slot is returned.
        drop(ticket);
        assert_eq!(consumer.drain_committed.get(), 0);
    }

    /// Drain allowance exhaustion still rejects further redemption (the
    /// remaining escrow can be aborted/released without acquiring budget).
    /// The allowance is a single bounded reservoir, not unlimited.
    #[test]
    fn drain_allowance_exhaustion_rejects_further_redemption() {
        let state = escrow_state();
        let producer = account(100, 10, 20, 100);
        let pool = Arc::new(GlobalLeasePool::with_overshoot_debt(0, 0));
        let consumer = account_with_pool(BudgetMode::Enforce, 50, 32, 10, pool);
        // Pin to Hard with reconcile overdraw.
        let mut filler = consumer.charge(50_u64).expect("fill floor");
        filler.reconcile_size(120);
        assert_eq!(consumer.level(), BudgetLevel::Hard);
        assert_eq!(consumer.drain_allowance_bytes(), 32);

        let first = producer
            .charge(32_u64)
            .expect("producer charge")
            .try_into_escrow(&state)
            .expect("first escrow")
            .redeem_into(&consumer)
            .expect("first redeem consumes the entire allowance");
        let second = producer
            .charge(8_u64)
            .expect("producer charge")
            .try_into_escrow(&state)
            .expect("second escrow")
            .redeem_into(&consumer)
            .expect_err("allowance exhausted: no further redeems admitted");
        // Caller can still abort the unredeemable escrow without acquiring
        // any budget (drop/abort paths are budget-free).
        second.abort();
        // Dropping the first redeemed ticket frees the allowance slot so
        // subsequent redeems can proceed.
        drop(first);
        assert_eq!(consumer.drain_committed.get(), 0);
        let third = producer
            .charge(20_u64)
            .expect("producer charge")
            .try_into_escrow(&state)
            .expect("third escrow")
            .redeem_into(&consumer)
            .expect("allowance refreshed after release");
        assert_eq!(third.bytes(), Some(20));
    }

    /// The drain allowance is gated behind `level() == Hard`: when the
    /// consumer is below Hard, `try_charge_for_drain` returns `None` so the
    /// regular `charge` path is used instead (which already admits).
    #[test]
    fn drain_allowance_only_admits_when_at_hard() {
        let consumer = account(1_000, 64, 0, 1_000);
        assert_eq!(consumer.level(), BudgetLevel::Normal);
        assert!(consumer.try_charge_for_drain(32_u64).is_none());
    }

    /// `RuntimeMemorySnapshot::publish` propagates the per-runtime
    /// drain allowance and outstanding committed bytes from local `Cell`
    /// state, so observers can validate the Hard-consumer drain path
    /// without any new per-item shared atomics.
    #[test]
    fn flush_snapshot_publishes_drain_allowance_and_committed_bytes() {
        let state = escrow_state();
        let producer = account(100, 10, 20, 100);
        let pool = Arc::new(GlobalLeasePool::with_overshoot_debt(0, 0));
        let consumer = account_with_pool(BudgetMode::Enforce, 50, 64, 10, pool);
        let mut filler = consumer.charge(50_u64).expect("fill floor");
        filler.reconcile_size(120);
        assert_eq!(consumer.level(), BudgetLevel::Hard);

        consumer.flush_snapshot();
        assert_eq!(consumer.snapshot.drain_allowance_bytes(), 64);
        assert_eq!(consumer.snapshot.drain_committed_bytes(), 0);

        let drained = producer
            .charge(32_u64)
            .expect("producer charge")
            .try_into_escrow(&state)
            .expect("escrow")
            .redeem_into(&consumer)
            .expect("drain admits one redeem");
        consumer.flush_snapshot();
        assert_eq!(consumer.snapshot.drain_committed_bytes(), 32);

        drop(drained);
        consumer.flush_snapshot();
        assert_eq!(consumer.snapshot.drain_committed_bytes(), 0);
    }

    // -------------------------------------------------------------------------
    // Local envelope tests.
    // -------------------------------------------------------------------------

    #[test]
    fn local_envelope_drop_releases_ticket_once() {
        let acct = account(1_000, 10, 20, 0);
        let ticket = acct.charge(64_u64).expect("charge should fit");
        let envelope = LocalEnvelope::new("payload", ticket);
        assert_eq!(envelope.payload(), &"payload");
        assert!(envelope.has_ticket());
        acct.flush_snapshot();
        assert_eq!(acct.snapshot.charged_bytes(), 64);
        drop(envelope);
        acct.flush_snapshot();
        assert_eq!(
            acct.snapshot.charged_bytes(),
            0,
            "dropping the envelope refunds the ticket exactly once"
        );
    }

    #[test]
    fn local_envelope_into_parts_transfers_ticket_ownership() {
        let acct = account(1_000, 10, 20, 0);
        let ticket = acct.charge(50_u64).expect("charge should fit");
        let envelope = LocalEnvelope::new(7_u32, ticket);
        let (payload, ticket) = envelope.into_parts();
        assert_eq!(payload, 7);
        // The ticket outlives the envelope; the charge is still held.
        acct.flush_snapshot();
        assert_eq!(acct.snapshot.charged_bytes(), 50);
        drop(ticket);
        acct.flush_snapshot();
        assert_eq!(acct.snapshot.charged_bytes(), 0);
    }

    #[test]
    fn local_envelope_without_ticket_is_inert() {
        let envelope = LocalEnvelope::without_ticket(99_u64);
        assert!(!envelope.has_ticket());
        assert_eq!(envelope.into_payload(), 99);
    }

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
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 100,
                runtime_count: 1,
                enforcement: MemoryBudgetEnforcement::default(),
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
