// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! A raw batch store that provides payload-type-indexed storage of Arrow
//! [`RecordBatch`]es without OTAP schema validation.
//!
//! [`RawBatchStore`] is the inner storage type used by the validated
//! [`OtapBatchStore`](super::OtapBatchStore) implementations (`Logs`, `Metrics`,
//! `Traces`). It can also be used directly by terminal consumers (e.g. the
//! Parquet exporter) that legitimately transform batches in ways that may not
//! conform to the OTAP wire-protocol schema.

use arrow::array::RecordBatch;

use crate::proto::opentelemetry::arrow::v1::ArrowPayloadType;

// ---------------------------------------------------------------------------
// Payload layouts
// ---------------------------------------------------------------------------

/// Compact per-signal position for a payload, independent of its protobuf value.
#[must_use]
pub const fn payload_position(payload_type: ArrowPayloadType) -> Option<usize> {
    match payload_type {
        ArrowPayloadType::Unknown => None,
        ArrowPayloadType::ResourceAttrs => Some(0),
        ArrowPayloadType::ScopeAttrs => Some(1),
        ArrowPayloadType::UnivariateMetrics
        | ArrowPayloadType::Logs
        | ArrowPayloadType::Spans
        | ArrowPayloadType::Profiles => Some(2),
        ArrowPayloadType::NumberDataPoints
        | ArrowPayloadType::LogAttrs
        | ArrowPayloadType::SpanAttrs
        | ArrowPayloadType::ProfileValueTypes => Some(3),
        ArrowPayloadType::SummaryDataPoints
        | ArrowPayloadType::SpanEvents
        | ArrowPayloadType::Samples => Some(4),
        ArrowPayloadType::HistogramDataPoints
        | ArrowPayloadType::SpanLinks
        | ArrowPayloadType::Stacks => Some(5),
        ArrowPayloadType::ExpHistogramDataPoints
        | ArrowPayloadType::SpanEventAttrs
        | ArrowPayloadType::StackLocations => Some(6),
        ArrowPayloadType::NumberDpAttrs
        | ArrowPayloadType::SpanLinkAttrs
        | ArrowPayloadType::ProfileLocations => Some(7),
        ArrowPayloadType::SummaryDpAttrs | ArrowPayloadType::ProfileLocationLines => Some(8),
        ArrowPayloadType::HistogramDpAttrs | ArrowPayloadType::ProfileFunctions => Some(9),
        ArrowPayloadType::ExpHistogramDpAttrs | ArrowPayloadType::ProfileMappings => Some(10),
        ArrowPayloadType::NumberDpExemplars | ArrowPayloadType::ProfileLinks => Some(11),
        ArrowPayloadType::HistogramDpExemplars | ArrowPayloadType::ProfileAttrs => Some(12),
        ArrowPayloadType::ExpHistogramDpExemplars | ArrowPayloadType::ProfileSampleAttrs => {
            Some(13)
        }
        ArrowPayloadType::NumberDpExemplarAttrs | ArrowPayloadType::ProfileMappingAttrs => Some(14),
        ArrowPayloadType::HistogramDpExemplarAttrs | ArrowPayloadType::ProfileLocationAttrs => {
            Some(15)
        }
        ArrowPayloadType::ExpHistogramDpExemplarAttrs => Some(16),
        ArrowPayloadType::MultivariateMetrics => Some(17),
        ArrowPayloadType::MetricAttrs => Some(18),
    }
}

// ---------------------------------------------------------------------------
// Constants -- layout identifiers and counts for each signal type
// ---------------------------------------------------------------------------

/// Logs payload layout identifier.
pub const LOGS_LAYOUT: u8 = 1;
/// Metrics payload layout identifier.
pub const METRICS_LAYOUT: u8 = 2;
/// Traces payload layout identifier.
pub const TRACES_LAYOUT: u8 = 3;
/// Profiles payload layout identifier.
pub const PROFILES_LAYOUT: u8 = 4;

/// Number of payload slots for the Logs signal.
pub const LOGS_COUNT: usize = 4;

/// Number of payload slots for the Metrics signal.
pub const METRICS_COUNT: usize = 19;

/// Number of payload slots for the Traces signal.
pub const TRACES_COUNT: usize = 8;

/// Number of payload slots for the Profiles signal, including shared payloads.
pub const PROFILES_COUNT: usize = 16;

// ---------------------------------------------------------------------------
// Type aliases
// ---------------------------------------------------------------------------

/// Raw (unvalidated) batch store for the Logs signal.
pub type RawLogsStore = RawBatchStore<LOGS_LAYOUT, LOGS_COUNT>;

/// Raw (unvalidated) batch store for the Metrics signal.
pub type RawMetricsStore = RawBatchStore<METRICS_LAYOUT, METRICS_COUNT>;

/// Raw (unvalidated) batch store for the Traces signal.
pub type RawTracesStore = RawBatchStore<TRACES_LAYOUT, TRACES_COUNT>;

/// Raw (unvalidated) batch store for the Profiles signal.
pub type RawProfilesStore = RawBatchStore<PROFILES_LAYOUT, PROFILES_COUNT>;

// ---------------------------------------------------------------------------
// RawBatchStore
// ---------------------------------------------------------------------------

/// A fixed-size, payload-type-indexed store of optional [`RecordBatch`]es.
///
/// The `LAYOUT` const generic selects the signal-local payload layout. The
/// `COUNT` const generic is the number of slots in the backing array.
///
/// This type provides **no** OTAP schema validation. Callers that need
/// validation should use the [`OtapBatchStore`](super::OtapBatchStore) trait
/// implementations which wrap this type.
#[derive(Clone, Debug, PartialEq)]
pub struct RawBatchStore<const LAYOUT: u8, const COUNT: usize> {
    batches: Box<[Option<RecordBatch>; COUNT]>,
}

impl<const LAYOUT: u8, const COUNT: usize> Default for RawBatchStore<LAYOUT, COUNT> {
    fn default() -> Self {
        Self {
            batches: Box::new(std::array::from_fn(|_| None)),
        }
    }
}

impl<const LAYOUT: u8, const COUNT: usize> RawBatchStore<LAYOUT, COUNT> {
    /// Create a new empty store with all slots set to `None`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a store from a pre-built batch array.
    #[must_use]
    pub fn from_batches(batches: [Option<RecordBatch>; COUNT]) -> Self {
        Self {
            batches: Box::new(batches),
        }
    }

    /// Check whether the given payload type is valid for this store.
    #[must_use]
    pub fn is_valid_type(payload_type: ArrowPayloadType) -> bool {
        match LAYOUT {
            LOGS_LAYOUT => matches!(
                payload_type,
                ArrowPayloadType::ResourceAttrs
                    | ArrowPayloadType::ScopeAttrs
                    | ArrowPayloadType::Logs
                    | ArrowPayloadType::LogAttrs
            ),
            METRICS_LAYOUT => matches!(
                payload_type,
                ArrowPayloadType::ResourceAttrs
                    | ArrowPayloadType::ScopeAttrs
                    | ArrowPayloadType::UnivariateMetrics
                    | ArrowPayloadType::NumberDataPoints
                    | ArrowPayloadType::SummaryDataPoints
                    | ArrowPayloadType::HistogramDataPoints
                    | ArrowPayloadType::ExpHistogramDataPoints
                    | ArrowPayloadType::NumberDpAttrs
                    | ArrowPayloadType::SummaryDpAttrs
                    | ArrowPayloadType::HistogramDpAttrs
                    | ArrowPayloadType::ExpHistogramDpAttrs
                    | ArrowPayloadType::NumberDpExemplars
                    | ArrowPayloadType::HistogramDpExemplars
                    | ArrowPayloadType::ExpHistogramDpExemplars
                    | ArrowPayloadType::NumberDpExemplarAttrs
                    | ArrowPayloadType::HistogramDpExemplarAttrs
                    | ArrowPayloadType::ExpHistogramDpExemplarAttrs
                    | ArrowPayloadType::MultivariateMetrics
                    | ArrowPayloadType::MetricAttrs
            ),
            TRACES_LAYOUT => matches!(
                payload_type,
                ArrowPayloadType::ResourceAttrs
                    | ArrowPayloadType::ScopeAttrs
                    | ArrowPayloadType::Spans
                    | ArrowPayloadType::SpanAttrs
                    | ArrowPayloadType::SpanEvents
                    | ArrowPayloadType::SpanLinks
                    | ArrowPayloadType::SpanEventAttrs
                    | ArrowPayloadType::SpanLinkAttrs
            ),
            PROFILES_LAYOUT => matches!(
                payload_type,
                ArrowPayloadType::ResourceAttrs
                    | ArrowPayloadType::ScopeAttrs
                    | ArrowPayloadType::Profiles
                    | ArrowPayloadType::ProfileValueTypes
                    | ArrowPayloadType::Samples
                    | ArrowPayloadType::Stacks
                    | ArrowPayloadType::StackLocations
                    | ArrowPayloadType::ProfileLocations
                    | ArrowPayloadType::ProfileLocationLines
                    | ArrowPayloadType::ProfileFunctions
                    | ArrowPayloadType::ProfileMappings
                    | ArrowPayloadType::ProfileLinks
                    | ArrowPayloadType::ProfileAttrs
                    | ArrowPayloadType::ProfileSampleAttrs
                    | ArrowPayloadType::ProfileMappingAttrs
                    | ArrowPayloadType::ProfileLocationAttrs
            ),
            _ => false,
        }
    }

    /// Read-only access to the underlying batch array as a slice.
    #[must_use]
    pub fn batches(&self) -> &[Option<RecordBatch>] {
        self.batches.as_slice()
    }

    /// Mutable access to the underlying batch array as a slice.
    pub fn batches_mut(&mut self) -> &mut [Option<RecordBatch>] {
        self.batches.as_mut_slice()
    }

    /// Consume the store and return the underlying batch array.
    #[must_use]
    pub fn into_batches(self) -> [Option<RecordBatch>; COUNT] {
        *self.batches
    }

    /// Get a reference to the batch for the given payload type, if present.
    ///
    /// Returns `None` if the payload type is not valid for this store or if
    /// no batch has been set for that slot.
    #[must_use]
    pub fn get(&self, payload_type: ArrowPayloadType) -> Option<&RecordBatch> {
        if !Self::is_valid_type(payload_type) {
            return None;
        }
        let idx = payload_position(payload_type).expect("valid payload has compact position");
        self.batches[idx].as_ref()
    }

    /// Set the batch for the given payload type.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if `payload_type` is not valid for this store.
    /// Callers must ensure the type is valid (see [`Self::is_valid_type`]).
    pub fn set(&mut self, payload_type: ArrowPayloadType, record_batch: RecordBatch) {
        debug_assert!(
            Self::is_valid_type(payload_type),
            "payload type {payload_type:?} is not valid for this store"
        );
        let idx = payload_position(payload_type).expect("valid payload has compact position");
        self.batches[idx] = Some(record_batch);
    }

    /// Remove the batch for the given payload type.
    ///
    /// # Panics
    ///
    /// Panics in debug builds if `payload_type` is not valid for this store.
    /// Callers must ensure the type is valid (see [`Self::is_valid_type`]).
    pub fn remove(&mut self, payload_type: ArrowPayloadType) {
        debug_assert!(
            Self::is_valid_type(payload_type),
            "payload type {payload_type:?} is not valid for this store"
        );
        let idx = payload_position(payload_type).expect("valid payload has compact position");
        self.batches[idx] = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_logs_store_basic_operations() {
        use arrow::array::UInt16Array;
        use arrow::datatypes::{DataType, Field, Schema};
        use std::sync::Arc;

        let mut store = RawLogsStore::new();

        // All slots start as None
        assert!(store.get(ArrowPayloadType::Logs).is_none());
        assert!(store.get(ArrowPayloadType::LogAttrs).is_none());

        // Invalid type returns None
        assert!(store.get(ArrowPayloadType::Spans).is_none());

        // Set a batch
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::UInt16, true)]));
        let batch =
            RecordBatch::try_new(schema, vec![Arc::new(UInt16Array::from(vec![1u16]))]).unwrap();
        store.set(ArrowPayloadType::Logs, batch);
        assert!(store.get(ArrowPayloadType::Logs).is_some());

        // Remove it
        store.remove(ArrowPayloadType::Logs);
        assert!(store.get(ArrowPayloadType::Logs).is_none());
    }

    /// Exhaustively verify that `is_valid_type` returns `true` for exactly
    /// the payload types listed in `allowed_payload_types()` for each signal,
    /// and `false` for every other known payload type.
    #[test]
    fn type_mask_matches_allowed_payload_types() {
        use crate::otap::{Logs, Metrics, OtapBatchStore, Traces};
        use std::collections::HashSet;

        // Union of all known payload types across all signals, plus Unknown.
        let all_types: HashSet<ArrowPayloadType> = std::iter::once(ArrowPayloadType::Unknown)
            .chain(Logs::allowed_payload_types().iter().copied())
            .chain(Metrics::allowed_payload_types().iter().copied())
            .chain(Traces::allowed_payload_types().iter().copied())
            .collect();

        let cases: &[(&str, fn(ArrowPayloadType) -> bool, &[ArrowPayloadType])] = &[
            (
                "Logs",
                RawLogsStore::is_valid_type,
                Logs::allowed_payload_types(),
            ),
            (
                "Metrics",
                RawMetricsStore::is_valid_type,
                Metrics::allowed_payload_types(),
            ),
            (
                "Traces",
                RawTracesStore::is_valid_type,
                Traces::allowed_payload_types(),
            ),
        ];

        for &(signal, is_valid, allowed) in cases {
            let allowed_set: HashSet<_> = allowed.iter().copied().collect();
            for &pt in &all_types {
                let expected = allowed_set.contains(&pt);
                assert_eq!(
                    is_valid(pt),
                    expected,
                    "{signal}: is_valid_type({pt:?}) should be {expected}"
                );
            }
        }
    }

    #[test]
    fn into_batches_returns_correct_length() {
        let store = RawLogsStore::new();
        assert_eq!(store.into_batches().len(), LOGS_COUNT);

        let store = RawMetricsStore::new();
        assert_eq!(store.into_batches().len(), METRICS_COUNT);

        let store = RawTracesStore::new();
        assert_eq!(store.into_batches().len(), TRACES_COUNT);
    }
}
