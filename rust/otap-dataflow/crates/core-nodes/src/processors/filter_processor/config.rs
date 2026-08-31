// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Implementation of the configuration of the filter processor
//!

use otel_arrow_dfe_pdata::otap::filter::{
    logs::LogFilter, metrics::MetricFilter, profiles::ProfileFilter, traces::TraceFilter,
};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default = "default_metric_filter")]
    metrics: MetricFilter,
    #[serde(default = "default_log_filter")]
    logs: LogFilter,
    #[serde(default = "default_trace_filter")]
    traces: TraceFilter,
    #[serde(default)]
    profiles: ProfileFilter,
}

/// create empty log filter as default value
const fn default_log_filter() -> LogFilter {
    LogFilter::new(None, None, vec![])
}

/// create empty metric filter as default value
const fn default_metric_filter() -> MetricFilter {
    MetricFilter::new(None, None)
}

/// create empty trace filter as default value
const fn default_trace_filter() -> TraceFilter {
    TraceFilter::new(None, None)
}

impl Config {
    pub fn new(logs: LogFilter, traces: TraceFilter) -> Self {
        Self {
            metrics: MetricFilter::new(None, None),
            logs,
            traces,
            profiles: ProfileFilter::default(),
        }
    }

    pub fn new_with_metrics(metrics: MetricFilter, logs: LogFilter, traces: TraceFilter) -> Self {
        Self {
            metrics,
            logs,
            traces,
            profiles: ProfileFilter::default(),
        }
    }

    #[must_use]
    pub const fn metric_filters(&self) -> &MetricFilter {
        &self.metrics
    }

    #[must_use]
    pub const fn log_filters(&self) -> &LogFilter {
        &self.logs
    }

    #[must_use]
    pub const fn trace_filters(&self) -> &TraceFilter {
        &self.traces
    }

    #[must_use]
    pub const fn profile_filters(&self) -> &ProfileFilter {
        &self.profiles
    }
}
