// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Engine-level metrics for the OTAP dataflow engine.
//!
//! Unlike per-pipeline metrics (which are sampled on each pipeline thread),
//! engine metrics are emitted **once per engine instance** by a dedicated
//! background task spawned by the controller.
//!
//! **Metrics**
//! - `memory_rss` (`ObserveUpDownCounter<u64>`, `{By}`):
//!   Process-wide Resident Set Size — physical memory currently held in RAM.
//!   Matches what external tools report (e.g. `kubectl top pod`, `htop`, `ps rss`).
//!
//! - `cpu_utilization` (`Gauge<f64>`, `{1}`):
//!   Process-wide CPU utilization as a ratio in `[0, 1]`, normalized across **all
//!   logical CPU cores on the system** (not just the cores assigned to the engine).
//!   Computed as `cpu_delta / (wall_delta × num_system_cores)` over the last
//!   measurement interval. A value of `1.0` means 100% of all system cores are
//!   in use; `0.5` on an 8-core machine corresponds to 4 fully loaded cores.
//!   Aligned with the OTel semantic convention `process.cpu.utilization`.
//!
//! - `memory_pressure_state` (`Gauge<u64>`, `{state}`):
//!   Process-wide memory limiter state encoded as `0=normal`, `1=soft`, `2=hard`.
//!
//! - `process_memory_usage_bytes`, `process_memory_soft_limit_bytes`,
//!   `process_memory_hard_limit_bytes` (`Gauge<u64>`, `{By}`):
//!   Process-wide memory limiter sample and effective limits.
//!
//!   We emit utilization directly (rather than a cumulative `cpu_time` counter)
//!   so that users can read the metric as-is without requiring PromQL `rate()`
//!   or similar query-time derivations.
//!
//!   TODO: Also emit a cumulative `cpu_time` counter (like the Go Collector's
//!   `process_cpu_seconds_total`) for users who prefer query-time computation.

use crate::memory_budget::{BudgetScope, MemoryBudgetState, RetainedSiteKind};
use crate::memory_limiter::MemoryPressureState;
use cpu_time::ProcessTime;
use otap_df_telemetry::instrument::{Gauge, ObserveUpDownCounter};
use otap_df_telemetry::metrics::MetricSet;
use otap_df_telemetry::registry::{EntityKey, TelemetryRegistryHandle};
use otap_df_telemetry::reporter::MetricsReporter;
use otap_df_telemetry_macros::{attribute_set, metric_set};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

/// Engine-wide metrics emitted once per engine instance.
#[metric_set(name = "engine")]
#[derive(Debug, Default, Clone)]
pub struct EngineMetrics {
    /// Process-wide Resident Set Size — physical RAM currently used by the process.
    /// Matches what external tools report (e.g. `kubectl top pod`, `htop`, `ps rss`).
    #[metric(unit = "{By}")]
    pub memory_rss: ObserveUpDownCounter<u64>,

    /// Process-wide CPU utilization as a ratio in [0, 1], normalized across all
    /// logical CPU cores on the system (not just engine-assigned cores).
    /// Aligned with the OTel semantic convention `process.cpu.utilization`.
    ///
    /// The `cpu.mode` attribute is not set; this reports combined user + system time.
    #[metric(unit = "{1}")]
    pub cpu_utilization: Gauge<f64>,

    /// Process-wide memory limiter state encoded as `0=normal`, `1=soft`, `2=hard`.
    #[metric(unit = "{state}")]
    pub memory_pressure_state: Gauge<u64>,

    /// Most recent process-wide memory limiter sample, in bytes.
    #[metric(unit = "{By}")]
    pub process_memory_usage_bytes: Gauge<u64>,

    /// Effective process-wide memory limiter soft limit, in bytes.
    #[metric(unit = "{By}")]
    pub process_memory_soft_limit_bytes: Gauge<u64>,

    /// Effective process-wide memory limiter hard limit, in bytes.
    #[metric(unit = "{By}")]
    pub process_memory_hard_limit_bytes: Gauge<u64>,

    /// Registered runtime memory-budget snapshot count.
    #[metric(unit = "{runtime}")]
    pub runtime_memory_budget_runtime_count: Gauge<u64>,

    /// Runtime memory-budget snapshots currently at normal level.
    #[metric(unit = "{runtime}")]
    pub runtime_memory_budget_normal_runtime_count: Gauge<u64>,

    /// Runtime memory-budget snapshots currently at soft level.
    #[metric(unit = "{runtime}")]
    pub runtime_memory_budget_soft_runtime_count: Gauge<u64>,

    /// Runtime memory-budget snapshots currently at hard level.
    #[metric(unit = "{runtime}")]
    pub runtime_memory_budget_hard_runtime_count: Gauge<u64>,

    /// Known logical retained bytes charged to runtime memory budgets.
    #[metric(unit = "{By}")]
    pub runtime_memory_budget_charged_bytes: Gauge<u64>,

    /// Lease bytes borrowed by runtime memory budgets.
    #[metric(unit = "{By}")]
    pub runtime_memory_budget_borrowed_bytes: Gauge<u64>,

    /// Retained bytes observed without a known logical size.
    #[metric(unit = "{By}")]
    pub runtime_memory_budget_unknown_bytes: Gauge<u64>,

    /// Retained item count observed without a known logical size.
    #[metric(unit = "{item}")]
    pub runtime_memory_budget_unknown_count: Gauge<u64>,

    /// Bytes above runtime floor plus leases.
    #[metric(unit = "{By}")]
    pub runtime_memory_budget_overshoot_bytes: Gauge<u64>,

    /// Reconciliation-debt bytes: overshoot recorded after growth that
    /// overdrew the global debt pool and remains unbacked.
    #[metric(unit = "{By}")]
    pub runtime_memory_budget_reconcile_debt_bytes: Gauge<u64>,

    /// Abandoned escrow tickets retained for leak detection.
    #[metric(unit = "{ticket}")]
    pub runtime_memory_budget_abandoned_escrow_count: Gauge<u64>,

    /// Abandoned escrow bytes retained for leak detection.
    #[metric(unit = "{By}")]
    pub runtime_memory_budget_abandoned_escrow_bytes: Gauge<u64>,

    /// Age of the oldest abandoned escrow entry retained for leak detection.
    #[metric(unit = "ms")]
    pub runtime_memory_budget_abandoned_escrow_oldest_age_millis: Gauge<u64>,

    /// Bounded abandoned-escrow alarms emitted by this engine.
    #[metric(unit = "{alarm}")]
    pub runtime_memory_budget_abandoned_escrow_alarm_count: Gauge<u64>,

    /// Abandoned-escrow entries whose metadata was compacted because the bounded
    /// tracking deque was full (cumulative). Non-zero means reaping precision was
    /// reduced; the leak remains counted in the abandoned totals.
    #[metric(unit = "{ticket}")]
    pub runtime_memory_budget_abandoned_escrow_compacted_count: Gauge<u64>,

    /// Abandoned-escrow entries reclaimed by the reaper (cumulative).
    #[metric(unit = "{ticket}")]
    pub runtime_memory_budget_reaped_escrow_count: Gauge<u64>,

    /// Bytes reclaimed from abandoned escrow by the reaper (cumulative).
    #[metric(unit = "{By}")]
    pub runtime_memory_budget_reaped_escrow_bytes: Gauge<u64>,

    /// Escrow tickets currently owning logical retained bytes.
    #[metric(unit = "{ticket}")]
    pub runtime_memory_budget_escrow_ticket_count: Gauge<u64>,

    /// Shared-boundary retained items whose exact logical size is unknown.
    #[metric(unit = "{item}")]
    pub runtime_memory_budget_escrow_unknown_count: Gauge<u64>,

    /// Escrow bytes currently owning logical retained bytes.
    #[metric(unit = "{By}")]
    pub runtime_memory_budget_escrow_charged_bytes: Gauge<u64>,

    /// Escrow buckets currently holding logical retained bytes.
    #[metric(unit = "{bucket}")]
    pub runtime_memory_budget_escrow_active_bucket_count: Gauge<u64>,

    /// Maximum logical retained bytes currently held by one escrow bucket.
    #[metric(unit = "{By}")]
    pub runtime_memory_budget_escrow_max_bucket_bytes: Gauge<u64>,

    /// Escrow bytes currently backed by an explicit borrow against the global
    /// spare pool (the pool-backed share of `escrow_charged_bytes`).
    #[metric(unit = "{By}")]
    pub runtime_memory_budget_escrow_pool_held_bytes: Gauge<u64>,

    /// Escrow bytes that could not be backed by a pool borrow at conversion
    /// time and are tolerated only in observe-only mode (enforce rejects them).
    #[metric(unit = "{By}")]
    pub runtime_memory_budget_escrow_pool_overshoot_bytes: Gauge<u64>,

    /// Drain/redemption allowance configured across runtimes.
    #[metric(unit = "{By}")]
    pub runtime_memory_budget_drain_allowance_bytes: Gauge<u64>,

    /// Drain/redemption bytes currently outstanding across runtimes.
    #[metric(unit = "{By}")]
    pub runtime_memory_budget_drain_committed_bytes: Gauge<u64>,

    /// Spare bytes available to lease from the global memory-budget pool.
    #[metric(unit = "{By}")]
    pub runtime_memory_budget_spare_available_bytes: Gauge<u64>,
}

/// Per-retention-site memory-budget attribution, emitted as one low-cardinality
/// labeled series per [`RetainedSiteKind`] (selected by the `site` attribute).
///
/// This is a breakdown of the aggregate `runtime_memory_budget_charged_bytes`
/// and `runtime_memory_budget_unknown_count` gauges: summing a field across all
/// `site` values reproduces the aggregate. It lets operators tell apart which
/// kind of retained work (batch buffers, retry buffers, parked routes, in-flight
/// exporter requests, etc.) is holding memory, without per-item telemetry on the
/// charge hot path — the values are read from a per-site snapshot at report time.
///
/// Topic queue/ring retention is owned by escrow buckets and surfaced by the
/// `runtime_memory_budget_escrow_*` gauges, so it does not appear here.
#[metric_set(name = "engine")]
#[derive(Debug, Default, Clone)]
pub struct RetainedSiteMetrics {
    /// Known logical retained bytes charged to this retention site.
    #[metric(unit = "{By}")]
    pub runtime_memory_budget_site_charged_bytes: Gauge<u64>,

    /// Retained item count observed without a known logical size at this site.
    #[metric(unit = "{item}")]
    pub runtime_memory_budget_site_unknown_count: Gauge<u64>,
}

/// Attribute set scoping a [`RetainedSiteMetrics`] series to one retention site.
///
/// The `site` value is the static, low-cardinality [`RetainedSiteKind::as_str`]
/// label, so constructing this set is allocation-free (`Cow::Borrowed`).
#[attribute_set(name = "engine.retained.site.attrs")]
#[derive(Debug, Clone, Default, Hash)]
pub struct RetainedSiteAttributeSet {
    /// Low-cardinality retention-site label (e.g. `batch_pending`).
    #[attribute(key = "site")]
    pub site: Cow<'static, str>,
}

/// Per-escrow-boundary attribution, emitted as one low-cardinality labeled
/// series per escrow boundary (selected by the `boundary` attribute).
///
/// This is a breakdown of the aggregate `runtime_memory_budget_escrow_charged_bytes`
/// and `runtime_memory_budget_escrow_unknown_count` gauges: summing a field
/// across all `boundary` values reproduces the aggregate (modulo boundaries that
/// are momentarily idle and therefore not emitted). It lets operators tell apart
/// which topic or shared edge owns escrowed retained work. The boundary set is
/// bounded by the pipeline configuration (topic names and shared edge ids), and
/// the values are read from a per-boundary snapshot at report time, so there is
/// no per-item metric work.
#[metric_set(name = "engine")]
#[derive(Debug, Default, Clone)]
pub struct EscrowBoundaryMetrics {
    /// Logical bytes currently owned by this boundary's escrow bucket.
    #[metric(unit = "{By}")]
    pub runtime_memory_budget_escrow_boundary_charged_bytes: Gauge<u64>,

    /// Unknown-size shared items currently retained against this boundary.
    #[metric(unit = "{item}")]
    pub runtime_memory_budget_escrow_boundary_unknown_count: Gauge<u64>,
}

/// Attribute set scoping an [`EscrowBoundaryMetrics`] series to one boundary.
///
/// The `boundary` value is an interned topic name or shared edge id, bounded by
/// the pipeline configuration. It is an owned label constructed once per
/// boundary when the series is first registered, never on the per-item path.
#[attribute_set(name = "engine.escrow.boundary.attrs")]
#[derive(Debug, Clone, Default, Hash)]
pub struct EscrowBoundaryAttributeSet {
    /// Low-cardinality escrow-boundary label (e.g. `topic:logs` or
    /// `receiver:otlp_grpc:default`).
    #[attribute(key = "boundary")]
    pub boundary: Cow<'static, str>,
}

/// Per-runtime/per-scope memory-budget attribution, emitted as one labeled
/// series per live runtime scope (pipeline group, pipeline, core, deployment
/// generation).
///
/// This is a scope breakdown of the aggregate runtime gauges
/// (`runtime_memory_budget_charged_bytes`, ...): summing a field across all
/// scopes reproduces the aggregate. It lets operators attribute retained memory
/// to a specific pipeline/runtime. The labels are bounded and stable (one
/// runtime per pinned core) and the values are read from per-runtime snapshots
/// at report time, so there is no per-item metric work and no per-item string
/// cloning.
#[metric_set(name = "engine")]
#[derive(Debug, Default, Clone)]
pub struct RuntimeScopeMetrics {
    /// Known logical retained bytes charged to this runtime scope.
    #[metric(unit = "{By}")]
    pub runtime_memory_budget_scope_charged_bytes: Gauge<u64>,

    /// Retained bytes observed without a known logical size in this scope.
    #[metric(unit = "{By}")]
    pub runtime_memory_budget_scope_unknown_bytes: Gauge<u64>,

    /// Retained item count observed without a known logical size in this scope.
    #[metric(unit = "{item}")]
    pub runtime_memory_budget_scope_unknown_count: Gauge<u64>,

    /// Bytes borrowed from the global lease pool by this runtime scope.
    #[metric(unit = "{By}")]
    pub runtime_memory_budget_scope_borrowed_bytes: Gauge<u64>,

    /// Bytes above the runtime floor plus leases in this scope.
    #[metric(unit = "{By}")]
    pub runtime_memory_budget_scope_overshoot_bytes: Gauge<u64>,

    /// Reconciliation-debt bytes for this runtime scope.
    #[metric(unit = "{By}")]
    pub runtime_memory_budget_scope_reconcile_debt_bytes: Gauge<u64>,

    /// Pressure level for this scope (`0=normal`, `1=soft`, `2=hard`).
    #[metric(unit = "{state}")]
    pub runtime_memory_budget_scope_level: Gauge<u64>,
}

/// Attribute set scoping a [`RuntimeScopeMetrics`] series to one runtime scope.
///
/// All four labels are bounded by the deployment topology (pipeline group,
/// pipeline, core, deployment generation). They are owned labels constructed
/// once per scope when the series is first registered, never on the per-item
/// path.
#[attribute_set(name = "engine.budget.scope.attrs")]
#[derive(Debug, Clone, Default, Hash)]
pub struct RuntimeScopeAttributeSet {
    /// Pipeline group owning the retained bytes.
    #[attribute(key = "pipeline_group_id")]
    pub pipeline_group_id: Cow<'static, str>,
    /// Pipeline owning the retained bytes.
    #[attribute(key = "pipeline_id")]
    pub pipeline_id: Cow<'static, str>,
    /// Core id of the runtime owning the retained bytes.
    #[attribute(key = "core_id")]
    pub core_id: Cow<'static, str>,
    /// Runtime deployment generation owning the retained bytes.
    #[attribute(key = "deployment_generation")]
    pub deployment_generation: Cow<'static, str>,
}

/// Monitors and reports engine-wide metrics.
///
/// Created by the controller and driven by a periodic timer in a dedicated
/// background task. Call [`update`](Self::update) to sample current values
/// and [`report`](Self::report) to flush them to the metrics pipeline.
pub struct EngineMetricsMonitor {
    metrics: MetricSet<EngineMetrics>,
    /// One labeled metric set per attributed retention site, paired with the
    /// site kind it reports. Updated from the per-site snapshot rollup.
    site_metrics: Vec<(RetainedSiteKind, MetricSet<RetainedSiteMetrics>)>,
    /// One labeled metric set per escrow boundary, registered lazily the first
    /// time a boundary reports activity and then reused. The boundary set is
    /// bounded by the pipeline configuration, so the map size is bounded.
    escrow_boundary_metrics: HashMap<Arc<str>, MetricSet<EscrowBoundaryMetrics>>,
    /// One labeled metric set per live runtime scope, registered lazily and
    /// pruned when the runtime is gone. Bounded by the number of runtimes.
    runtime_scope_metrics: HashMap<BudgetScope, MetricSet<RuntimeScopeMetrics>>,
    reporter: MetricsReporter,
    registry: TelemetryRegistryHandle,
    /// Wall-clock anchor for the current measurement interval.
    wall_start: Instant,
    /// Process-wide CPU time anchor for the current measurement interval.
    cpu_start: ProcessTime,
    /// Total number of logical CPU cores available on the system.
    num_cores: usize,
    /// Shared process-wide memory limiter state.
    memory_pressure_state: MemoryPressureState,
    /// Shared runtime memory-budget state.
    memory_budget_state: MemoryBudgetState,
}

impl EngineMetricsMonitor {
    /// Creates a new engine metrics monitor.
    ///
    /// The caller must have already registered the engine entity via
    /// [`ControllerContext::register_engine_entity`](crate::context::ControllerContext::register_engine_entity).
    #[must_use]
    pub fn new(
        registry: TelemetryRegistryHandle,
        entity_key: EntityKey,
        reporter: MetricsReporter,
        memory_pressure_state: MemoryPressureState,
        memory_budget_state: MemoryBudgetState,
    ) -> Self {
        let metrics = registry.register_metric_set_for_entity::<EngineMetrics>(entity_key);
        // One labeled metric set per attributed retention site. Constructing the
        // attribute set is allocation-free because the `site` value is a static
        // `&'static str` from `RetainedSiteKind::as_str`.
        let site_metrics = RetainedSiteKind::METRIC_SITES
            .iter()
            .map(|&site| {
                let metric_set =
                    registry.register_metric_set::<RetainedSiteMetrics>(RetainedSiteAttributeSet {
                        site: Cow::Borrowed(site.as_str()),
                    });
                (site, metric_set)
            })
            .collect();
        let num_cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        Self {
            metrics,
            site_metrics,
            escrow_boundary_metrics: HashMap::new(),
            runtime_scope_metrics: HashMap::new(),
            reporter,
            registry,
            wall_start: Instant::now(),
            cpu_start: ProcessTime::now(),
            num_cores,
            memory_pressure_state,
            memory_budget_state,
        }
    }

    /// Samples current engine-wide metrics (RSS, CPU utilization, etc.).
    pub fn update(&mut self) {
        self.metrics.memory_rss.observe(get_rss_bytes());

        // Compute process-wide CPU utilization normalized across all cores.
        let now_wall = Instant::now();
        let now_cpu = ProcessTime::now();
        let wall_delta = now_wall.duration_since(self.wall_start);
        let cpu_delta = now_cpu.duration_since(self.cpu_start);
        let wall_secs = wall_delta.as_secs_f64();
        if wall_secs > 0.0 {
            let utilization =
                (cpu_delta.as_secs_f64() / (wall_secs * self.num_cores as f64)).clamp(0.0, 1.0);
            self.metrics.cpu_utilization.set(utilization);
        } else {
            self.metrics.cpu_utilization.set(0.0);
        }
        self.metrics
            .memory_pressure_state
            .set(self.memory_pressure_state.level() as u64);
        self.metrics
            .process_memory_usage_bytes
            .set(self.memory_pressure_state.usage_bytes());
        self.metrics
            .process_memory_soft_limit_bytes
            .set(self.memory_pressure_state.soft_limit_bytes());
        self.metrics
            .process_memory_hard_limit_bytes
            .set(self.memory_pressure_state.hard_limit_bytes());
        let memory_budget = self.memory_budget_state.snapshot();
        self.metrics
            .runtime_memory_budget_runtime_count
            .set(memory_budget.runtime_count);
        self.metrics
            .runtime_memory_budget_normal_runtime_count
            .set(memory_budget.normal_runtime_count);
        self.metrics
            .runtime_memory_budget_soft_runtime_count
            .set(memory_budget.soft_runtime_count);
        self.metrics
            .runtime_memory_budget_hard_runtime_count
            .set(memory_budget.hard_runtime_count);
        self.metrics
            .runtime_memory_budget_charged_bytes
            .set(memory_budget.charged_bytes);
        self.metrics
            .runtime_memory_budget_borrowed_bytes
            .set(memory_budget.borrowed_bytes);
        self.metrics
            .runtime_memory_budget_unknown_bytes
            .set(memory_budget.unknown_bytes);
        self.metrics
            .runtime_memory_budget_unknown_count
            .set(memory_budget.unknown_count);
        self.metrics
            .runtime_memory_budget_overshoot_bytes
            .set(memory_budget.overshoot_bytes);
        self.metrics
            .runtime_memory_budget_reconcile_debt_bytes
            .set(memory_budget.reconcile_debt_bytes);
        self.metrics
            .runtime_memory_budget_abandoned_escrow_count
            .set(memory_budget.abandoned_escrow_count);
        self.metrics
            .runtime_memory_budget_abandoned_escrow_bytes
            .set(memory_budget.abandoned_escrow_bytes);
        self.metrics
            .runtime_memory_budget_abandoned_escrow_oldest_age_millis
            .set(memory_budget.abandoned_escrow_oldest_age_millis);
        self.metrics
            .runtime_memory_budget_abandoned_escrow_alarm_count
            .set(memory_budget.abandoned_escrow_alarm_count);
        self.metrics
            .runtime_memory_budget_abandoned_escrow_compacted_count
            .set(memory_budget.abandoned_escrow_compacted_count);
        self.metrics
            .runtime_memory_budget_reaped_escrow_count
            .set(memory_budget.reaped_escrow_count);
        self.metrics
            .runtime_memory_budget_reaped_escrow_bytes
            .set(memory_budget.reaped_escrow_bytes);
        self.metrics
            .runtime_memory_budget_escrow_ticket_count
            .set(memory_budget.escrow_ticket_count);
        self.metrics
            .runtime_memory_budget_escrow_unknown_count
            .set(memory_budget.escrow_unknown_count);
        self.metrics
            .runtime_memory_budget_escrow_charged_bytes
            .set(memory_budget.escrow_charged_bytes);
        self.metrics
            .runtime_memory_budget_escrow_active_bucket_count
            .set(memory_budget.escrow_active_bucket_count);
        self.metrics
            .runtime_memory_budget_escrow_max_bucket_bytes
            .set(memory_budget.escrow_max_bucket_bytes);
        self.metrics
            .runtime_memory_budget_escrow_pool_held_bytes
            .set(memory_budget.escrow_pool_held_bytes);
        self.metrics
            .runtime_memory_budget_escrow_pool_overshoot_bytes
            .set(memory_budget.escrow_pool_overshoot_bytes);
        self.metrics
            .runtime_memory_budget_drain_allowance_bytes
            .set(memory_budget.drain_allowance_bytes);
        self.metrics
            .runtime_memory_budget_drain_committed_bytes
            .set(memory_budget.drain_committed_bytes);
        self.metrics
            .runtime_memory_budget_spare_available_bytes
            .set(memory_budget.spare_available_bytes);
        // Per-retention-site attribution: read the per-site rollup from the same
        // snapshot so the labeled series stay consistent with the aggregates.
        for (site, metric_set) in self.site_metrics.iter_mut() {
            let index = site.index();
            metric_set
                .runtime_memory_budget_site_charged_bytes
                .set(memory_budget.charged_bytes_by_site[index]);
            metric_set
                .runtime_memory_budget_site_unknown_count
                .set(memory_budget.unknown_count_by_site[index]);
        }
        self.update_escrow_boundary_metrics();
        self.update_runtime_scope_metrics();
        self.wall_start = now_wall;
        self.cpu_start = now_cpu;
    }

    /// Refreshes the per-escrow-boundary labeled series from the boundary
    /// snapshot. Boundaries that have gone idle since the last sample are zeroed
    /// (their cached series is retained but reports `0`), and newly active
    /// boundaries register a series lazily. Bounded by the pipeline's boundary
    /// set, so the cache and per-update work stay bounded.
    fn update_escrow_boundary_metrics(&mut self) {
        // Zero all cached series first so boundaries absent from this round's
        // snapshot report 0 rather than a stale value.
        for metric_set in self.escrow_boundary_metrics.values_mut() {
            metric_set
                .runtime_memory_budget_escrow_boundary_charged_bytes
                .set(0);
            metric_set
                .runtime_memory_budget_escrow_boundary_unknown_count
                .set(0);
        }
        // Disjoint field borrows: the registration closure only touches
        // `registry`, while the entry borrows `escrow_boundary_metrics`.
        let registry = &self.registry;
        let boundary_metrics = &mut self.escrow_boundary_metrics;
        for snapshot in self.memory_budget_state.escrow_bucket_snapshots() {
            let metric_set = boundary_metrics
                .entry(Arc::clone(&snapshot.boundary))
                .or_insert_with(|| {
                    registry.register_metric_set::<EscrowBoundaryMetrics>(
                        EscrowBoundaryAttributeSet {
                            boundary: Cow::Owned(snapshot.boundary.to_string()),
                        },
                    )
                });
            metric_set
                .runtime_memory_budget_escrow_boundary_charged_bytes
                .set(snapshot.charged_bytes);
            metric_set
                .runtime_memory_budget_escrow_boundary_unknown_count
                .set(snapshot.unknown_count);
        }
    }

    /// Refreshes the per-runtime/per-scope labeled series from the scope
    /// snapshot. Scopes whose runtime has gone away are unregistered and dropped
    /// from the cache so it stays bounded by the live runtime count; live scopes
    /// register a series lazily and report their current values. The aggregate
    /// runtime gauges are emitted separately and unchanged.
    fn update_runtime_scope_metrics(&mut self) {
        let scope_snapshots = self.memory_budget_state.runtime_scope_snapshots();
        // Prune cached series for scopes whose runtime is no longer live so the
        // cache never accumulates stale per-scope series across redeployments.
        let registry = &self.registry;
        self.runtime_scope_metrics.retain(|scope, metric_set| {
            let live = scope_snapshots.iter().any(|s| &s.scope == scope);
            if !live {
                let _ = registry.unregister_metric_set(metric_set.metric_set_key());
            }
            live
        });
        // Disjoint field borrows: the registration closure only touches
        // `registry`, while the entry borrows `runtime_scope_metrics`.
        let scope_metrics = &mut self.runtime_scope_metrics;
        for snapshot in &scope_snapshots {
            let metric_set = scope_metrics
                .entry(Arc::clone(&snapshot.scope))
                .or_insert_with(|| {
                    registry.register_metric_set::<RuntimeScopeMetrics>(scope_attribute_set(
                        &snapshot.scope,
                    ))
                });
            metric_set
                .runtime_memory_budget_scope_charged_bytes
                .set(snapshot.charged_bytes);
            metric_set
                .runtime_memory_budget_scope_unknown_bytes
                .set(snapshot.unknown_bytes);
            metric_set
                .runtime_memory_budget_scope_unknown_count
                .set(snapshot.unknown_count);
            metric_set
                .runtime_memory_budget_scope_borrowed_bytes
                .set(snapshot.borrowed_bytes);
            metric_set
                .runtime_memory_budget_scope_overshoot_bytes
                .set(snapshot.overshoot_bytes);
            metric_set
                .runtime_memory_budget_scope_reconcile_debt_bytes
                .set(snapshot.reconcile_debt_bytes);
            metric_set
                .runtime_memory_budget_scope_level
                .set(snapshot.level);
        }
    }

    /// Flushes sampled metrics to the reporting pipeline.
    ///
    /// Returns an error only if the metrics channel is permanently closed.
    /// A full channel is silently tolerated (non-blocking, try-send semantics).
    pub fn report(&mut self) -> Result<(), otap_df_telemetry::error::Error> {
        self.reporter.report(&mut self.metrics)?;
        for (_site, metric_set) in self.site_metrics.iter_mut() {
            self.reporter.report(metric_set)?;
        }
        for metric_set in self.escrow_boundary_metrics.values_mut() {
            self.reporter.report(metric_set)?;
        }
        for metric_set in self.runtime_scope_metrics.values_mut() {
            self.reporter.report(metric_set)?;
        }
        Ok(())
    }
}

/// Returns the current process-wide RSS (Resident Set Size) in bytes.
fn get_rss_bytes() -> u64 {
    memory_stats::memory_stats()
        .map(|stats| stats.physical_mem as u64)
        .unwrap_or(0)
}

/// Builds the owned, bounded attribute set for a runtime scope series.
///
/// Missing labels map to an empty string. Constructed once per scope when the
/// series is first registered, never on the per-item path.
fn scope_attribute_set(scope: &BudgetScope) -> RuntimeScopeAttributeSet {
    RuntimeScopeAttributeSet {
        pipeline_group_id: scope
            .pipeline_group_id
            .clone()
            .map_or(Cow::Borrowed(""), Cow::Owned),
        pipeline_id: scope
            .pipeline_id
            .clone()
            .map_or(Cow::Borrowed(""), Cow::Owned),
        core_id: scope
            .core_id
            .map_or(Cow::Borrowed(""), |id| Cow::Owned(id.to_string())),
        deployment_generation: scope
            .runtime_generation
            .map_or(Cow::Borrowed(""), |generation| {
                Cow::Owned(generation.to_string())
            }),
    }
}

impl Drop for EngineMetricsMonitor {
    fn drop(&mut self) {
        let _ = self
            .registry
            .unregister_metric_set(self.metrics.metric_set_key());
        for (_site, metric_set) in &self.site_metrics {
            let _ = self
                .registry
                .unregister_metric_set(metric_set.metric_set_key());
        }
        for metric_set in self.escrow_boundary_metrics.values() {
            let _ = self
                .registry
                .unregister_metric_set(metric_set.metric_set_key());
        }
        for metric_set in self.runtime_scope_metrics.values() {
            let _ = self
                .registry
                .unregister_metric_set(metric_set.metric_set_key());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ControllerContext;
    use otap_df_telemetry::metrics::{MetricSetHandler, MetricSetSnapshot, MetricValue};
    use otap_df_telemetry::registry::TelemetryRegistryHandle;

    fn engine_metric(snapshot: &MetricSetSnapshot, name: &str) -> MetricValue {
        let index = EngineMetrics::default()
            .descriptor()
            .metrics
            .iter()
            .position(|field| field.name == name)
            .unwrap_or_else(|| panic!("engine metric {name} should exist"));
        snapshot.get_metrics()[index]
    }

    #[test]
    fn engine_metrics_reports_nonzero_rss() {
        let registry = TelemetryRegistryHandle::new();
        let controller = ControllerContext::new(registry.clone());
        let entity_key = controller.register_engine_entity();
        let (_rx, reporter) = MetricsReporter::create_new_and_receiver(16);

        let mut monitor = EngineMetricsMonitor::new(
            registry,
            entity_key,
            reporter,
            controller.memory_pressure_state(),
            controller.memory_budget_state(),
        );
        monitor.update();

        assert!(
            monitor.metrics.memory_rss.get() > 0,
            "memory_rss should report non-zero process RSS"
        );
    }

    #[test]
    fn engine_metrics_report_succeeds() {
        let registry = TelemetryRegistryHandle::new();
        let controller = ControllerContext::new(registry.clone());
        let entity_key = controller.register_engine_entity();
        let (_rx, reporter) = MetricsReporter::create_new_and_receiver(16);

        let mut monitor = EngineMetricsMonitor::new(
            registry,
            entity_key,
            reporter,
            controller.memory_pressure_state(),
            controller.memory_budget_state(),
        );
        monitor.update();
        assert!(monitor.report().is_ok());
    }

    #[test]
    fn engine_metrics_cpu_utilization_in_range() {
        let registry = TelemetryRegistryHandle::new();
        let controller = ControllerContext::new(registry.clone());
        let entity_key = controller.register_engine_entity();
        let (_rx, reporter) = MetricsReporter::create_new_and_receiver(16);

        let mut monitor = EngineMetricsMonitor::new(
            registry,
            entity_key,
            reporter,
            controller.memory_pressure_state(),
            controller.memory_budget_state(),
        );

        // Do a small busy-spin so there is measurable CPU time.
        let start = Instant::now();
        while start.elapsed() < std::time::Duration::from_millis(10) {
            let _ = std::hint::black_box(0u64.wrapping_add(1));
        }

        monitor.update();
        let util = monitor.metrics.cpu_utilization.get();
        assert!(
            (0.0..=1.0).contains(&util),
            "cpu_utilization should be in [0, 1], got {util}"
        );
    }

    #[test]
    fn engine_metrics_expose_process_memory_limiter_usage_and_limits() {
        let registry = TelemetryRegistryHandle::new();
        let controller = ControllerContext::new(registry.clone());
        let state = controller.memory_pressure_state();
        state.configure(crate::memory_limiter::MemoryPressureBehaviorConfig {
            retry_after_secs: 1,
            fail_readiness_on_hard: true,
            mode: otap_df_config::policy::MemoryLimiterMode::Enforce,
        });
        state.set_sample_for_tests(
            crate::memory_limiter::MemoryPressureLevel::Soft,
            95,
            90,
            100,
        );

        let entity_key = controller.register_engine_entity();
        let (_rx, reporter) = MetricsReporter::create_new_and_receiver(16);
        let mut monitor = EngineMetricsMonitor::new(
            registry,
            entity_key,
            reporter,
            state,
            controller.memory_budget_state(),
        );

        monitor.update();

        assert_eq!(monitor.metrics.memory_pressure_state.get(), 1);
        assert_eq!(monitor.metrics.process_memory_usage_bytes.get(), 95);
        assert_eq!(monitor.metrics.process_memory_soft_limit_bytes.get(), 90);
        assert_eq!(monitor.metrics.process_memory_hard_limit_bytes.get(), 100);
    }

    #[test]
    fn engine_metrics_expose_runtime_memory_budget_snapshot() {
        let registry = TelemetryRegistryHandle::new();
        let controller = ControllerContext::new(registry.clone());
        controller.configure_memory_budget(
            crate::memory_budget::RuntimeMemoryBudgetConfig {
                mode: crate::memory_budget::BudgetMode::ObserveOnly,
                retry_after_secs: 1,
                sizing: crate::memory_budget::MemoryBudgetSizing {
                    reserve_bytes: 0,
                    floor_per_runtime_bytes: 100,
                    lease_step_bytes: 10,
                    max_overshoot_per_runtime_bytes: 20,
                    overshoot_debt_limit_bytes: 10,
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 100,
                runtime_count: 1,
                enforcement: crate::memory_budget::MemoryBudgetEnforcement::default(),
            },
            None,
        );
        let handle = controller
            .memory_budget_state()
            .register_runtime_snapshot(crate::memory_budget::BudgetScopeId::default());
        let account = handle.local_account().expect("budget should be configured");
        let _ticket = account
            .charge(115_u64)
            .expect("overshoot should be observed");
        let _escrow = account
            .charge(30_u64)
            .expect("escrow source charge should fit")
            .try_into_escrow(&controller.memory_budget_state())
            .expect("observe-only escrow should fit");
        let abandoned_escrow = account
            .charge(12_u64)
            .expect("abandoned escrow source charge should fit")
            .try_into_escrow(&controller.memory_budget_state())
            .expect("observe-only abandoned escrow should fit");
        drop(abandoned_escrow);
        struct UnknownSize;
        impl crate::memory_budget::ChargedSize for UnknownSize {
            fn charged_size(&self) -> Option<u64> {
                None
            }
        }
        let unknown_shared = handle
            .shared_escrow_minter("metrics:unknown".into())
            .mint_slot(UnknownSize);
        // The charge crossed a level threshold (Normal -> Soft) which publishes
        // automatically, but a final flush keeps the test resilient to future
        // changes that defer the transition publish.
        account.flush_snapshot();

        let entity_key = controller.register_engine_entity();
        let (rx, reporter) = MetricsReporter::create_new_and_receiver(16);
        let mut monitor = EngineMetricsMonitor::new(
            registry,
            entity_key,
            reporter,
            controller.memory_pressure_state(),
            controller.memory_budget_state(),
        );

        monitor.update();

        assert_eq!(monitor.metrics.runtime_memory_budget_runtime_count.get(), 1);
        assert_eq!(
            monitor.metrics.runtime_memory_budget_charged_bytes.get(),
            115
        );
        assert_eq!(
            monitor.metrics.runtime_memory_budget_overshoot_bytes.get(),
            15
        );
        assert_eq!(
            monitor
                .metrics
                .runtime_memory_budget_escrow_charged_bytes
                .get(),
            42
        );
        assert_eq!(
            monitor
                .metrics
                .runtime_memory_budget_escrow_unknown_count
                .get(),
            1
        );
        assert_eq!(
            monitor
                .metrics
                .runtime_memory_budget_escrow_active_bucket_count
                .get(),
            1
        );
        assert_eq!(
            monitor
                .metrics
                .runtime_memory_budget_escrow_max_bucket_bytes
                .get(),
            42
        );
        assert_eq!(
            monitor
                .metrics
                .runtime_memory_budget_abandoned_escrow_count
                .get(),
            1
        );
        assert_eq!(
            monitor
                .metrics
                .runtime_memory_budget_abandoned_escrow_bytes
                .get(),
            12
        );
        assert!(
            monitor
                .metrics
                .runtime_memory_budget_abandoned_escrow_oldest_age_millis
                .get()
                >= 1
        );
        assert_eq!(
            monitor
                .metrics
                .runtime_memory_budget_abandoned_escrow_alarm_count
                .get(),
            1
        );
        assert_eq!(
            monitor
                .metrics
                .runtime_memory_budget_soft_runtime_count
                .get(),
            1
        );
        // The drain allowance defaults to `lease_step_bytes` (10) via the
        // local_account wiring, and nothing is drained yet.
        assert_eq!(
            monitor
                .metrics
                .runtime_memory_budget_drain_allowance_bytes
                .get(),
            10
        );
        assert_eq!(
            monitor
                .metrics
                .runtime_memory_budget_drain_committed_bytes
                .get(),
            0
        );

        monitor
            .report()
            .expect("engine metrics report should succeed");
        let snapshot = rx
            .try_recv()
            .expect("engine metrics snapshot should be emitted");
        assert_eq!(
            engine_metric(&snapshot, "runtime.memory.budget.soft.runtime.count"),
            MetricValue::U64(1),
            "reported soft runtime count should reflect threshold crossing"
        );
        assert_eq!(
            engine_metric(&snapshot, "runtime.memory.budget.charged.bytes"),
            MetricValue::U64(115),
            "reported charged bytes should include retained runtime budget charge"
        );
        assert_eq!(
            engine_metric(&snapshot, "runtime.memory.budget.overshoot.bytes"),
            MetricValue::U64(15),
            "reported overshoot bytes should include bytes above floor plus leases"
        );
        assert_eq!(
            engine_metric(&snapshot, "runtime.memory.budget.escrow.unknown.count"),
            MetricValue::U64(1),
            "reported escrow unknown count should include active shared unknown owners"
        );
        drop(unknown_shared);
    }

    #[test]
    fn engine_metrics_expose_per_retention_site_attribution() {
        let registry = TelemetryRegistryHandle::new();
        let controller = ControllerContext::new(registry.clone());
        controller.configure_memory_budget(
            crate::memory_budget::RuntimeMemoryBudgetConfig {
                mode: crate::memory_budget::BudgetMode::ObserveOnly,
                retry_after_secs: 1,
                sizing: crate::memory_budget::MemoryBudgetSizing {
                    reserve_bytes: 0,
                    floor_per_runtime_bytes: 1_000,
                    lease_step_bytes: 10,
                    max_overshoot_per_runtime_bytes: 20,
                    overshoot_debt_limit_bytes: 10,
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 1_000,
                runtime_count: 1,
                enforcement: crate::memory_budget::MemoryBudgetEnforcement::default(),
            },
            None,
        );
        let handle = controller
            .memory_budget_state()
            .register_runtime_snapshot(crate::memory_budget::BudgetScopeId::default());
        let account = handle.local_account().expect("budget should be configured");
        let _batch = account
            .charge_at(RetainedSiteKind::BatchPending, 50_u64)
            .expect("batch charge should fit");
        let _retry = account
            .charge_at(RetainedSiteKind::RetryBuffer, 30_u64)
            .expect("retry charge should fit");
        account.flush_snapshot();

        // The labeled metric set must declare the expected metric names.
        let descriptor = RetainedSiteMetrics::default().descriptor();
        assert!(
            descriptor
                .metrics
                .iter()
                .any(|f| f.name == "runtime.memory.budget.site.charged.bytes"),
            "per-site charged-bytes metric must exist"
        );
        assert!(
            descriptor
                .metrics
                .iter()
                .any(|f| f.name == "runtime.memory.budget.site.unknown.count"),
            "per-site unknown-count metric must exist"
        );

        let entity_key = controller.register_engine_entity();
        let (rx, reporter) = MetricsReporter::create_new_and_receiver(64);
        let mut monitor = EngineMetricsMonitor::new(
            registry,
            entity_key,
            reporter,
            controller.memory_pressure_state(),
            controller.memory_budget_state(),
        );
        monitor.update();

        // One labeled series per emitted site kind.
        assert_eq!(
            monitor.site_metrics.len(),
            RetainedSiteKind::METRIC_SITES.len()
        );
        let site_charged = |site: RetainedSiteKind| -> u64 {
            monitor
                .site_metrics
                .iter()
                .find(|(s, _)| *s == site)
                .map(|(_, m)| m.runtime_memory_budget_site_charged_bytes.get())
                .expect("site metric set should exist")
        };
        assert_eq!(site_charged(RetainedSiteKind::BatchPending), 50);
        assert_eq!(site_charged(RetainedSiteKind::RetryBuffer), 30);
        assert_eq!(site_charged(RetainedSiteKind::FanoutInflight), 0);

        // The aggregate gauge is unchanged, and the per-site series sum back to
        // it: attribution is a breakdown, not an extra total.
        assert_eq!(
            monitor.metrics.runtime_memory_budget_charged_bytes.get(),
            80
        );
        let per_site_sum: u64 = monitor
            .site_metrics
            .iter()
            .map(|(_, m)| m.runtime_memory_budget_site_charged_bytes.get())
            .sum();
        assert_eq!(per_site_sum, 80);

        // The batch_pending series is emitted to the reporting pipeline with the
        // right value; match it by metric-set key so the assertion is robust.
        let batch_key = monitor
            .site_metrics
            .iter()
            .find(|(s, _)| *s == RetainedSiteKind::BatchPending)
            .map(|(_, m)| m.metric_set_key())
            .expect("batch_pending metric set");
        let charged_index = descriptor
            .metrics
            .iter()
            .position(|f| f.name == "runtime.memory.budget.site.charged.bytes")
            .expect("charged-bytes metric index");

        monitor.report().expect("report should succeed");
        let mut batch_value = None;
        while let Ok(snapshot) = rx.try_recv() {
            if snapshot.key() == batch_key {
                batch_value = Some(snapshot.get_metrics()[charged_index]);
            }
        }
        assert_eq!(
            batch_value,
            Some(MetricValue::U64(50)),
            "reported batch_pending site series should carry the charged bytes"
        );
    }

    #[test]
    fn engine_metrics_expose_per_scope_attribution() {
        let registry = TelemetryRegistryHandle::new();
        let controller = ControllerContext::new(registry.clone());
        controller.configure_memory_budget(
            crate::memory_budget::RuntimeMemoryBudgetConfig {
                mode: crate::memory_budget::BudgetMode::ObserveOnly,
                retry_after_secs: 1,
                sizing: crate::memory_budget::MemoryBudgetSizing {
                    reserve_bytes: 0,
                    floor_per_runtime_bytes: 1_000,
                    lease_step_bytes: 10,
                    max_overshoot_per_runtime_bytes: 20,
                    overshoot_debt_limit_bytes: 10,
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 1_000,
                runtime_count: 1,
                enforcement: crate::memory_budget::MemoryBudgetEnforcement::default(),
            },
            None,
        );
        let handle = controller.memory_budget_state().register_runtime_snapshot(
            crate::memory_budget::BudgetScopeId {
                pipeline_group_id: Some("grp".to_owned()),
                pipeline_id: Some("pipe-a".to_owned()),
                core_id: Some(0),
                runtime_generation: Some(3),
                topic_or_boundary: None,
            },
        );
        let account = handle.local_account().expect("budget should be configured");
        let _batch = account
            .charge_at(RetainedSiteKind::BatchPending, 64_u64)
            .expect("charge should fit");
        account.flush_snapshot();

        // The labeled metric set declares the expected stable metric names.
        let descriptor = RuntimeScopeMetrics::default().descriptor();
        assert!(
            descriptor
                .metrics
                .iter()
                .any(|f| f.name == "runtime.memory.budget.scope.charged.bytes"),
            "per-scope charged-bytes metric must exist"
        );

        let entity_key = controller.register_engine_entity();
        let (_rx, reporter) = MetricsReporter::create_new_and_receiver(64);
        let mut monitor = EngineMetricsMonitor::new(
            registry,
            entity_key,
            reporter,
            controller.memory_pressure_state(),
            controller.memory_budget_state(),
        );
        monitor.update();

        assert_eq!(
            monitor.runtime_scope_metrics.len(),
            1,
            "one labeled series per live runtime scope"
        );
        let scope_charged: u64 = monitor
            .runtime_scope_metrics
            .values()
            .map(|m| m.runtime_memory_budget_scope_charged_bytes.get())
            .sum();
        assert_eq!(scope_charged, 64);
        // The aggregate is unchanged and the per-scope series sum back to it.
        assert_eq!(
            scope_charged,
            monitor.metrics.runtime_memory_budget_charged_bytes.get()
        );
        monitor.report().expect("report should succeed");
    }

    #[test]
    fn engine_metrics_expose_per_boundary_escrow_attribution() {
        let registry = TelemetryRegistryHandle::new();
        let controller = ControllerContext::new(registry.clone());
        controller.configure_memory_budget(
            crate::memory_budget::RuntimeMemoryBudgetConfig {
                mode: crate::memory_budget::BudgetMode::ObserveOnly,
                retry_after_secs: 1,
                sizing: crate::memory_budget::MemoryBudgetSizing {
                    reserve_bytes: 0,
                    floor_per_runtime_bytes: 1,
                    lease_step_bytes: 1,
                    max_overshoot_per_runtime_bytes: 1,
                    overshoot_debt_limit_bytes: 0,
                    drain_allowance_bytes: 0,
                },
                topic_default_limit_bytes: 1_000,
                runtime_count: 1,
                enforcement: crate::memory_budget::MemoryBudgetEnforcement::default(),
            },
            Some(1_000),
        );
        let handle = controller
            .memory_budget_state()
            .register_runtime_snapshot(crate::memory_budget::BudgetScopeId::default());
        let minter = handle.shared_escrow_minter(Arc::<str>::from("topic:logs"));
        let _owner = minter.mint(48_u64).expect("escrow mint should succeed");

        let entity_key = controller.register_engine_entity();
        let (_rx, reporter) = MetricsReporter::create_new_and_receiver(64);
        let mut monitor = EngineMetricsMonitor::new(
            registry,
            entity_key,
            reporter,
            controller.memory_pressure_state(),
            controller.memory_budget_state(),
        );
        monitor.update();

        assert_eq!(
            monitor.escrow_boundary_metrics.len(),
            1,
            "one labeled series for the single active boundary"
        );
        let boundary_charged: u64 = monitor
            .escrow_boundary_metrics
            .values()
            .map(|m| m.runtime_memory_budget_escrow_boundary_charged_bytes.get())
            .sum();
        assert_eq!(boundary_charged, 48);
        // The aggregate escrow gauge is unchanged and equals the per-boundary sum.
        assert_eq!(
            boundary_charged,
            monitor
                .metrics
                .runtime_memory_budget_escrow_charged_bytes
                .get()
        );
        monitor.report().expect("report should succeed");
    }
}
