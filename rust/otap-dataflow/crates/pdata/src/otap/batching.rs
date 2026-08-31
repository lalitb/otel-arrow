// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Batching for `OtapArrowRecords`

use super::{OtapArrowRecords, error::Result, groups::RecordsGroup};
use otel_arrow_dfe_config::SignalType;
use std::num::NonZeroU64;

/// Rebatch records to the appropriate size in a single pass, measured
/// in items.  Requires all inputs have the same signal type.
pub fn make_item_batches(
    signal: SignalType,
    max_items: Option<NonZeroU64>,
    records: Vec<OtapArrowRecords>,
) -> Result<Vec<OtapArrowRecords>> {
    // Separate by signal type.
    let mut records = match signal {
        SignalType::Logs => RecordsGroup::separate_logs(records),
        SignalType::Metrics => RecordsGroup::separate_metrics(records),
        SignalType::Traces => RecordsGroup::separate_traces(records),
        SignalType::Profiles => RecordsGroup::separate_profiles(records),
    }?;

    // Split large batches so they can be reassembled into
    // limited-size batches.
    if let Some(limit) = max_items {
        records = records.split(limit)?;
    }

    // Join batches in sequence.
    records = records.concatenate(max_items)?;
    records.into_otap_arrow_records()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::{
        ArrayRef, Int64Array, LargeListArray, RecordBatch, UInt32Array, UInt64Array,
    };
    use arrow::buffer::{OffsetBuffer, ScalarBuffer};
    use arrow::datatypes::{DataType, Field, Schema};

    use super::*;
    use crate::error::Error;
    use crate::otap::{OtapBatchStore, Profiles};
    use crate::proto::opentelemetry::arrow::v1::ArrowPayloadType;
    use crate::schema::consts;

    fn profile_records(profile_count: u32) -> OtapArrowRecords {
        let profile_ids: Vec<_> = (1..=profile_count).collect();
        let root = RecordBatch::try_from_iter([
            (
                consts::ID,
                Arc::new(UInt32Array::from(profile_ids)) as ArrayRef,
            ),
            (
                consts::TIME_UNIX_NANO,
                Arc::new(UInt64Array::from(vec![1; profile_count as usize])) as ArrayRef,
            ),
            (
                consts::DURATION_NANO,
                Arc::new(UInt64Array::from(vec![1; profile_count as usize])) as ArrayRef,
            ),
        ])
        .unwrap();

        let values = LargeListArray::new(
            Arc::new(Field::new("item", DataType::Int64, true)),
            OffsetBuffer::from_lengths([1]),
            Arc::new(Int64Array::new(ScalarBuffer::from(vec![1]), None)),
            None,
        );
        let timestamps = LargeListArray::new(
            Arc::new(Field::new("item", DataType::UInt64, true)),
            OffsetBuffer::from_lengths([1]),
            Arc::new(UInt64Array::new(ScalarBuffer::from(vec![1]), None)),
            None,
        );
        let samples = RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new(consts::ID, DataType::UInt32, false),
                Field::new(consts::PARENT_ID, DataType::UInt32, false),
                Field::new(
                    consts::VALUES,
                    DataType::LargeList(Arc::new(Field::new("item", DataType::Int64, true))),
                    false,
                ),
                Field::new(
                    consts::TIMESTAMPS_UNIX_NANO,
                    DataType::LargeList(Arc::new(Field::new("item", DataType::UInt64, true))),
                    false,
                ),
            ])),
            vec![
                Arc::new(UInt32Array::from(vec![10])),
                Arc::new(UInt32Array::from(vec![1])),
                Arc::new(values),
                Arc::new(timestamps),
            ],
        )
        .unwrap();
        let samples = crate::otap::testing::mark_id_columns_plain(samples);

        let mut profiles = Profiles::default();
        profiles.set(ArrowPayloadType::Profiles, root).unwrap();
        profiles.set(ArrowPayloadType::Samples, samples).unwrap();
        OtapArrowRecords::Profiles(profiles.validate().unwrap())
    }

    /// Scenario: Multiple valid Profiles BARs fit within the configured item limit.
    /// Guarantees: Rebatching preserves BAR boundaries until graph-aware merging exists.
    #[test]
    fn profiles_batching_preserves_input_boundaries() {
        let input = vec![profile_records(1), profile_records(1)];
        let expected = input.clone();

        let output = make_item_batches(
            SignalType::Profiles,
            Some(NonZeroU64::new(10).unwrap()),
            input,
        )
        .unwrap();

        assert_eq!(output, expected);
    }

    /// Scenario: One Profiles BAR exceeds the configured root-profile item limit.
    /// Guarantees: Rebatching returns a typed error instead of splitting the reference graph.
    #[test]
    fn profiles_batching_rejects_oversized_bar() {
        let error = make_item_batches(
            SignalType::Profiles,
            Some(NonZeroU64::new(1).unwrap()),
            vec![profile_records(2)],
        )
        .unwrap_err();

        assert!(matches!(
            error,
            Error::TooManyItems {
                payload_type: ArrowPayloadType::Profiles,
                count: 2,
                max: 1,
                ..
            }
        ));
    }
}
