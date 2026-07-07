// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Telemetry types for the listener-group manager.
//!
//! These metrics are emitted from controller startup and manager
//! materialisation paths. They intentionally do not sit on the
//! receiver listener hot path.

use crate::listener_group::ListenerGroupKey;
use otap_df_telemetry::instrument::Counter;
use otap_df_telemetry::metrics::MetricSet;
use otap_df_telemetry::registry::TelemetryRegistryHandle;
use otap_df_telemetry::reporter::MetricsReporter;
use otap_df_telemetry_macros::{attribute_set, metric_set};
use std::borrow::Cow;
use std::collections::HashMap;

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
    /// Number of groups that were materialised and ready for receiver acquire.
    #[metric(unit = "{group}")]
    pub groups_ready: Counter<u64>,
    /// Number of ready groups with a selector attached.
    #[metric(unit = "{group}")]
    pub selector_attached: Counter<u64>,
    /// Number of ready groups that degraded to plain `SO_REUSEPORT`.
    #[metric(unit = "{group}")]
    pub selector_fallback: Counter<u64>,
    /// Number of groups that fell back to independent listener
    /// creation for any reason.
    #[metric(unit = "{group}")]
    pub fallback_total: Counter<u64>,
    /// Number of groups whose materialisation (eager bind/listen)
    /// failed and surfaced as an `io::Error` to the first acquirer.
    #[metric(unit = "{group}")]
    pub materialisation_failed: Counter<u64>,
}

/// Listener-group lifecycle event recorded by the controller.
#[derive(Clone, Copy, Debug)]
pub enum ListenerGroupMetricEvent {
    /// Plan registration succeeded.
    PlanRegistered,
    /// Eager materialisation produced a ready group.
    GroupReady,
    /// A ready group has the eBPF selector attached.
    SelectorAttached,
    /// A ready group degraded to plain `SO_REUSEPORT`.
    SelectorFallback,
    /// Group fell back to independent binds.
    Fallback,
    /// Bind/listen/attach materialisation failed.
    MaterialisationFailed,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ListenerGroupMetricKey {
    pipeline_group_id: String,
    bind_addr: String,
    protocol: &'static str,
}

impl ListenerGroupMetricKey {
    fn from_group_key(key: &ListenerGroupKey) -> Self {
        Self {
            pipeline_group_id: key.pipeline_group_id.clone(),
            bind_addr: key.addr.to_string(),
            protocol: key.protocol.as_str(),
        }
    }

    fn attrs(&self) -> ListenerGroupAttributeSet {
        ListenerGroupAttributeSet {
            pipeline_group_id: self.pipeline_group_id.clone().into(),
            bind_addr: self.bind_addr.clone().into(),
            protocol: Cow::Borrowed(self.protocol),
        }
    }
}

/// Emits low-frequency listener-group lifecycle metrics.
pub struct ListenerGroupMetricsEmitter {
    registry: TelemetryRegistryHandle,
    reporter: MetricsReporter,
    metrics: HashMap<ListenerGroupMetricKey, MetricSet<ListenerGroupMetrics>>,
}

impl ListenerGroupMetricsEmitter {
    /// Creates a new emitter backed by the engine telemetry registry.
    #[must_use]
    pub fn new(registry: TelemetryRegistryHandle, reporter: MetricsReporter) -> Self {
        Self {
            registry,
            reporter,
            metrics: HashMap::new(),
        }
    }

    /// Records and flushes one listener-group lifecycle event.
    pub fn record(&mut self, key: &ListenerGroupKey, event: ListenerGroupMetricEvent) {
        let metric_key = ListenerGroupMetricKey::from_group_key(key);
        let metrics = self.metrics.entry(metric_key.clone()).or_insert_with(|| {
            self.registry
                .register_metric_set::<ListenerGroupMetrics>(metric_key.attrs())
        });
        match event {
            ListenerGroupMetricEvent::PlanRegistered => metrics.plans_registered.inc(),
            ListenerGroupMetricEvent::GroupReady => metrics.groups_ready.inc(),
            ListenerGroupMetricEvent::SelectorAttached => metrics.selector_attached.inc(),
            ListenerGroupMetricEvent::SelectorFallback => metrics.selector_fallback.inc(),
            ListenerGroupMetricEvent::Fallback => metrics.fallback_total.inc(),
            ListenerGroupMetricEvent::MaterialisationFailed => metrics.materialisation_failed.inc(),
        }
        let _ = self.reporter.report(metrics);
    }
}
