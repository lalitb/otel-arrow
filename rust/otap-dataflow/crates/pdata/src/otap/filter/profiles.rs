// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Sample filtering for OTAP Profiles.

use arrow::array::{BooleanArray, BooleanBufferBuilder};
use arrow::buffer::BooleanBuffer;
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::otap::OtapArrowRecords;
use crate::otap::transform::profiles::{
    ProfilesCompactionOptions, ProfilesTransformLimits, compact_profile_dimensions,
    filter_profile_samples,
};
use crate::proto::opentelemetry::arrow::v1::ArrowPayloadType;
use crate::schema::consts;

use super::{
    KeyValue, MatchType, build_id_filter, default_match_type, get_attr_filter, get_ids,
    nulls_to_false,
};

/// Overall requirements for filtering Profiles samples.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileFilter {
    /// Samples must match these properties to remain in the pipeline.
    include: Option<ProfileMatchProperties>,
    /// Samples matching these properties are removed from the pipeline.
    exclude: Option<ProfileMatchProperties>,
    /// Remove shared dimensions that become unreachable after filtering.
    compact: bool,
    /// Rewrite retained entity IDs densely when compaction is enabled.
    dense_ids: bool,
    /// Bounded output limits for filtering and optional compaction.
    limits: ProfilesTransformLimits,
}

/// Profile sample properties that can be matched safely without graph splitting.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileMatchProperties {
    /// Strict or regular-expression matching for string values.
    match_type: MatchType,
    /// Sample-owned attributes; a sample matches when any configured entry matches.
    sample_attributes: Vec<KeyValue>,
}

impl Default for ProfileMatchProperties {
    fn default() -> Self {
        Self {
            match_type: default_match_type(),
            sample_attributes: Vec::new(),
        }
    }
}

impl ProfileMatchProperties {
    /// Create sample-attribute match properties.
    #[must_use]
    pub const fn new(match_type: MatchType, sample_attributes: Vec<KeyValue>) -> Self {
        Self {
            match_type,
            sample_attributes,
        }
    }

    fn create_filter(&self, profiles: &OtapArrowRecords, invert: bool) -> Result<BooleanArray> {
        let samples =
            profiles
                .get(ArrowPayloadType::Samples)
                .ok_or(Error::RecordBatchNotFound {
                    payload_type: ArrowPayloadType::Samples,
                })?;
        let mut filter = if self.sample_attributes.is_empty() {
            BooleanArray::from(BooleanBuffer::new_set(samples.num_rows()))
        } else if let Some(attrs) = profiles.get(ArrowPayloadType::ProfileSampleAttrs) {
            let attrs_filter = get_attr_filter(
                profiles,
                &self.sample_attributes,
                &self.match_type,
                ArrowPayloadType::ProfileSampleAttrs,
            )?;
            let parent_ids =
                attrs
                    .column_by_name(consts::PARENT_ID)
                    .ok_or_else(|| Error::ColumnNotFound {
                        name: consts::PARENT_ID.to_string(),
                    })?;
            let matching_ids = get_ids(parent_ids, &attrs_filter)?;
            let sample_ids =
                samples
                    .column_by_name(consts::ID)
                    .ok_or_else(|| Error::ColumnNotFound {
                        name: consts::ID.to_string(),
                    })?;
            build_id_filter(sample_ids, matching_ids)?
        } else {
            BooleanArray::from(BooleanBuffer::new_unset(samples.num_rows()))
        };

        filter = nulls_to_false(&filter);
        if invert {
            arrow::compute::not(&filter).map_err(|source| Error::ColumnLengthMismatch { source })
        } else {
            Ok(filter)
        }
    }
}

impl ProfileFilter {
    /// Create a Profiles sample filter.
    #[must_use]
    pub const fn new(
        include: Option<ProfileMatchProperties>,
        exclude: Option<ProfileMatchProperties>,
        compact: bool,
        dense_ids: bool,
        limits: ProfilesTransformLimits,
    ) -> Self {
        Self {
            include,
            exclude,
            compact,
            dense_ids,
            limits,
        }
    }

    /// Validate that every configured sample criterion has a supported scalar value.
    pub fn validate(&self) -> Result<()> {
        for properties in [&self.include, &self.exclude].into_iter().flatten() {
            for attribute in &properties.sample_attributes {
                if matches!(
                    &attribute.value,
                    super::AnyValue::Array(_) | super::AnyValue::KeyValue(_)
                ) {
                    return Err(Error::UnsupportedProfilesTransform {
                        operation: "Profiles sample filtering supports only string, int, double, and bool values",
                    });
                }
            }
        }
        Ok(())
    }

    /// Filter sample rows and return `(output, samples_consumed, samples_dropped)`.
    pub fn filter(&self, profiles: OtapArrowRecords) -> Result<(OtapArrowRecords, u64, u64)> {
        self.validate()?;
        let samples =
            profiles
                .get(ArrowPayloadType::Samples)
                .ok_or(Error::RecordBatchNotFound {
                    payload_type: ArrowPayloadType::Samples,
                })?;
        let samples_before = u64::try_from(samples.num_rows()).unwrap_or(u64::MAX);
        if self.include.is_none() && self.exclude.is_none() && !self.compact {
            return Ok((profiles, samples_before, 0));
        }

        let selection = match (&self.include, &self.exclude) {
            (Some(include), Some(exclude)) => {
                let include = include.create_filter(&profiles, false)?;
                let exclude = exclude.create_filter(&profiles, true)?;
                arrow::compute::and_kleene(&include, &exclude)
                    .map_err(|source| Error::ColumnLengthMismatch { source })?
            }
            (Some(include), None) => include.create_filter(&profiles, false)?,
            (None, Some(exclude)) => exclude.create_filter(&profiles, true)?,
            (None, None) => {
                let mut builder = BooleanBufferBuilder::new(samples.num_rows());
                builder.append_n(samples.num_rows(), true);
                BooleanArray::new(builder.finish(), None)
            }
        };

        let (mut output, stats) = filter_profile_samples(&profiles, &selection, self.limits)?;
        if self.compact {
            (output, _) = compact_profile_dimensions(
                &output,
                ProfilesCompactionOptions {
                    dense_ids: self.dense_ids,
                },
                self.limits,
            )?;
        }

        Ok((output, samples_before, stats.samples_dropped()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::encode_profiles_otap_batch;
    use crate::otap::filter::AnyValue;
    use crate::testing::profiles::{ProfilesDatasetKind, profiles_dataset};

    fn high_cardinality_profiles() -> OtapArrowRecords {
        encode_profiles_otap_batch(&profiles_dataset(
            ProfilesDatasetKind::HighCardinalityAttributes,
            1,
            3,
            2,
        ))
        .unwrap()
    }

    /// Scenario: A strict sample-owned attribute include rule selects one profile sample.
    /// Guarantees: Only the matching sample and its attributes remain in the Profiles graph.
    #[test]
    fn includes_samples_by_sample_attribute() {
        let filter = ProfileFilter::new(
            Some(ProfileMatchProperties::new(
                MatchType::Strict,
                vec![KeyValue {
                    key: "sample.id".to_string(),
                    value: AnyValue::String("0:1".to_string()),
                }],
            )),
            None,
            false,
            false,
            ProfilesTransformLimits::default(),
        );

        let (filtered, consumed, dropped) = filter.filter(high_cardinality_profiles()).unwrap();

        assert_eq!(consumed, 3);
        assert_eq!(dropped, 2);
        assert_eq!(
            filtered.get(ArrowPayloadType::Samples).unwrap().num_rows(),
            1
        );
        assert_eq!(
            filtered
                .get(ArrowPayloadType::ProfileSampleAttrs)
                .unwrap()
                .num_rows(),
            1
        );
    }

    /// Scenario: Profiles filtering has no include or exclude rules but requests compaction.
    /// Guarantees: Samples pass unchanged and explicit compaction remains valid and bounded.
    #[test]
    fn no_filter_rules_preserve_samples() {
        let filter = ProfileFilter::new(None, None, true, true, ProfilesTransformLimits::default());

        let (filtered, consumed, dropped) = filter.filter(high_cardinality_profiles()).unwrap();

        assert_eq!(consumed, 3);
        assert_eq!(dropped, 0);
        assert_eq!(
            filtered.get(ArrowPayloadType::Samples).unwrap().num_rows(),
            3
        );
    }

    /// Scenario: A Profiles filter has no rules, no compaction, and limits below the input size.
    /// Guarantees: The compatibility no-op path returns the original BAR without rebuilding it.
    #[test]
    fn unconfigured_filter_is_a_true_passthrough() {
        let profiles = high_cardinality_profiles();
        let original = profiles.clone();
        let filter = ProfileFilter::new(
            None,
            None,
            false,
            false,
            ProfilesTransformLimits {
                max_output_rows: 0,
                max_output_bytes: 0,
                max_cloned_rows: 0,
            },
        );

        let (output, consumed, dropped) = filter.filter(profiles).unwrap();

        assert_eq!(output, original);
        assert_eq!(consumed, 3);
        assert_eq!(dropped, 0);
    }

    /// Scenario: A Profiles sample filter is configured with a complex array value.
    /// Guarantees: Unsupported criteria are rejected instead of silently dropping samples.
    #[test]
    fn rejects_unsupported_complex_filter_values() {
        let filter = ProfileFilter::new(
            Some(ProfileMatchProperties::new(
                MatchType::Strict,
                vec![KeyValue {
                    key: "sample.id".to_string(),
                    value: AnyValue::Array(vec![AnyValue::Int(1)]),
                }],
            )),
            None,
            false,
            false,
            ProfilesTransformLimits::default(),
        );

        assert!(matches!(
            filter.validate(),
            Err(Error::UnsupportedProfilesTransform { .. })
        ));
    }

    /// Scenario: A string criterion targets sample attributes whose adaptive schema has no string column.
    /// Guarantees: The absent value type produces no matches rather than a schema error.
    #[test]
    fn absent_optional_string_column_is_an_all_false_match() {
        let mut data = profiles_dataset(ProfilesDatasetKind::Cpu, 1, 2, 2);
        data.dictionary.as_mut().unwrap().attribute_table[1].value = Some(
            crate::proto::opentelemetry::common::v1::AnyValue::new_bool(true),
        );
        let profiles = encode_profiles_otap_batch(&data).unwrap();
        let filter = ProfileFilter::new(
            Some(ProfileMatchProperties::new(
                MatchType::Strict,
                vec![KeyValue {
                    key: "profile.kind".to_string(),
                    value: AnyValue::String("cpu".to_string()),
                }],
            )),
            None,
            false,
            false,
            ProfilesTransformLimits::default(),
        );

        let (output, consumed, dropped) = filter.filter(profiles).unwrap();

        assert_eq!(consumed, 2);
        assert_eq!(dropped, 2);
        assert_eq!(output.get(ArrowPayloadType::Samples).unwrap().num_rows(), 0);
    }
}
