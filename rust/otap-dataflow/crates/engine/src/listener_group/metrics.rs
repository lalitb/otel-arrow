// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Telemetry types for the listener-group manager.
//!
//! **Status: prepared, not yet emitted.** The type definitions below
//! describe the metric set and attributes that the future Phase 2.5
//! wiring will report. `ListenerGroupManager` does not increment any
//! of these counters today; nothing in the engine currently constructs
//! a `MetricSet<ListenerGroupMetrics>`. Wiring is deferred to the same
//! follow-up that integrates the manager into
//! `EffectHandler::tcp_listener` so the counters reflect a real
//! production code path.

use otap_df_telemetry::instrument::Counter;
use otap_df_telemetry_macros::{attribute_set, metric_set};
use std::borrow::Cow;

/// Attribute set identifying a single listener group instance.
#[attribute_set(name = "engine.listener_group.attrs")]
#[derive(Clone, Debug, Default, Hash)]
pub struct ListenerGroupAttributeSet {
    /// Pipeline group id that owns this listener group.
    #[attribute]
    pub pipeline_group_id: Cow<'static, str>,
    /// Bind address as a string (e.g. "0.0.0.0:4317"). Cardinality is
    /// bounded by the deployment configuration.
    #[attribute]
    pub bind_addr: Cow<'static, str>,
    /// Protocol identifier ("tcp" or "udp").
    #[attribute]
    pub protocol: Cow<'static, str>,
}

/// Counters tracking listener-group lifecycle events.
#[metric_set(name = "engine.listener_group")]
#[derive(Clone, Debug, Default)]
pub struct ListenerGroupMetrics {
    /// Number of plans registered with the manager.
    #[metric(unit = "{plan}")]
    pub plans_registered: Counter<u64>,
    /// Number of groups that reached quorum (all expected members
    /// acquired their listener).
    #[metric(unit = "{group}")]
    pub groups_ready: Counter<u64>,
    /// Number of groups that timed out waiting for quorum and fell
    /// back to independent listener creation.
    #[metric(unit = "{group}")]
    pub fallback_timeout: Counter<u64>,
    /// Number of groups whose materialisation (eager bind/listen)
    /// failed and surfaced as an `io::Error` to the first acquirer.
    #[metric(unit = "{group}")]
    pub materialisation_failed: Counter<u64>,
}
