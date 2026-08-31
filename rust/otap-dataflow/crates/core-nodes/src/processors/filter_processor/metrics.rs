// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Metrics for the OTAP FilterProcessor node.
use otel_arrow_dfe_telemetry::common_attributes::SignalAttributes;
use otel_arrow_dfe_telemetry::instrument::Counter;
use otel_arrow_dfe_telemetry_macros::metric_set;

/// Pdata-oriented metrics for the OTAP FilterProcessor
#[metric_set(
    name = "processor.filter.pdata",
    measurement_attributes = SignalAttributes
)]
#[derive(Debug, Default, Clone)]
pub struct FilterPdataMetrics {
    /// Number of signal items (log records, spans, or metric data points) a
    /// decision node chose to drop.
    #[metric(name = "dropped.items", unit = "{item}")]
    pub dropped_items: Counter<u64>,

    /// Number of Profiles sample rows removed without dropping their owning profile.
    #[metric(name = "dropped.samples", unit = "{sample}")]
    pub dropped_samples: Counter<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scenario: Profiles filtering removes samples without removing a root profile.
    /// Guarantees: Sample drops are counted separately from signal-item drops.
    #[test]
    fn separates_profile_sample_drops_from_signal_items() {
        let mut metrics = FilterPdataMetrics::default();
        metrics.dropped_samples.add(2);

        assert_eq!(metrics.dropped_items.get(), 0);
        assert_eq!(metrics.dropped_samples.get(), 2);
    }
}
