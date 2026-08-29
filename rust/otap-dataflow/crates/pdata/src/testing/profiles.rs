// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Deterministic Profiles datasets for tests and benchmarks.

use std::collections::HashMap;

use crate::proto::opentelemetry::common::v1::{AnyValue, InstrumentationScope, KeyValue};
use crate::proto::opentelemetry::profiles::v1development::{
    Function, KeyValueAndUnit, Line, Link, Location, Mapping, Profile, ProfilesData,
    ProfilesDictionary, ResourceProfiles, Sample, ScopeProfiles, Stack, ValueType,
};
use crate::proto::opentelemetry::resource::v1::Resource;

/// Representative profiling workload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProfilesDatasetKind {
    /// CPU samples with value observations.
    Cpu,
    /// Allocation-size samples.
    Allocation,
    /// Off-CPU samples with paired values and timestamps.
    OffCpu,
    /// Timestamp-only samples.
    TimestampOnly,
    /// Samples with a unique attribute value per sample.
    HighCardinalityAttributes,
    /// CPU samples carrying original pprof payload bytes.
    OriginalPayload,
}

impl ProfilesDatasetKind {
    /// Stable benchmark/test label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Allocation => "allocation",
            Self::OffCpu => "off_cpu",
            Self::TimestampOnly => "timestamp_only",
            Self::HighCardinalityAttributes => "high_cardinality_attributes",
            Self::OriginalPayload => "original_payload",
        }
    }
}

/// Build a deterministic Profiles dataset with shared symbol and stack tables.
#[must_use]
pub fn profiles_dataset(
    kind: ProfilesDatasetKind,
    profile_count: usize,
    samples_per_profile: usize,
    stack_depth: usize,
) -> ProfilesData {
    let mut dictionary = FixtureDictionary::new();
    let sample_type = match kind {
        ProfilesDatasetKind::Allocation => "allocated_space",
        ProfilesDatasetKind::OffCpu => "off_cpu",
        _ => "cpu",
    };
    let sample_unit = match kind {
        ProfilesDatasetKind::Allocation => "bytes",
        _ => "nanoseconds",
    };
    let sample_type_index = dictionary.intern_string(sample_type);
    let sample_unit_index = dictionary.intern_string(sample_unit);
    let period_type_index = dictionary.intern_string("period");
    let mapping_filename = dictionary.intern_string("fixture.bin");
    let common_attribute =
        dictionary.push_attribute("profile.kind", AnyValue::new_string(kind.as_str()), None);

    dictionary.dictionary.mapping_table.push(Mapping {
        memory_start: 0x1000,
        memory_limit: 0x2000,
        file_offset: 0,
        filename_strindex: mapping_filename,
        attribute_indices: vec![common_attribute],
    });

    let stack_depth = stack_depth.max(1);
    for depth in 0..stack_depth {
        let function_name = dictionary.intern_string(&format!("fixture_function_{depth}"));
        let filename = dictionary.intern_string(&format!("fixture_{depth}.rs"));
        dictionary.dictionary.function_table.push(Function {
            name_strindex: function_name,
            filename_strindex: filename,
            start_line: i64::try_from(depth + 1).expect("bounded fixture depth"),
            ..Default::default()
        });
        dictionary.dictionary.location_table.push(Location {
            mapping_index: 1,
            address: 0x1000 + u64::try_from(depth).expect("bounded fixture depth") * 16,
            lines: vec![Line {
                function_index: i32::try_from(depth + 1).expect("bounded fixture depth"),
                line: i64::try_from(depth + 1).expect("bounded fixture depth"),
                column: 1,
            }],
            attribute_indices: vec![common_attribute],
        });
    }
    dictionary.dictionary.stack_table.push(Stack {
        location_indices: (1..=stack_depth)
            .map(|index| i32::try_from(index).expect("bounded fixture depth"))
            .collect(),
    });
    dictionary.dictionary.link_table.push(Link {
        trace_id: vec![1; 16],
        span_id: vec![2; 8],
    });

    let profiles = (0..profile_count)
        .map(|profile_index| {
            let samples = (0..samples_per_profile)
                .map(|sample_index| {
                    let observation = i64::try_from(sample_index + 1).expect("bounded fixture");
                    let attribute_indices =
                        if kind == ProfilesDatasetKind::HighCardinalityAttributes {
                            vec![dictionary.push_attribute(
                                "sample.id",
                                AnyValue::new_string(format!("{profile_index}:{sample_index}")),
                                None,
                            )]
                        } else {
                            vec![common_attribute]
                        };
                    let (values, timestamps_unix_nano) = match kind {
                        ProfilesDatasetKind::TimestampOnly => (
                            Vec::new(),
                            vec![1_000_000 + u64::try_from(sample_index).expect("bounded fixture")],
                        ),
                        ProfilesDatasetKind::OffCpu => (
                            vec![observation * 100],
                            vec![1_000_000 + u64::try_from(sample_index).expect("bounded fixture")],
                        ),
                        ProfilesDatasetKind::Allocation => (vec![observation * 4096], Vec::new()),
                        _ => (vec![observation * 10], Vec::new()),
                    };
                    Sample {
                        stack_index: 1,
                        attribute_indices,
                        link_index: (sample_index % 2 == 0) as i32,
                        values,
                        timestamps_unix_nano,
                    }
                })
                .collect();
            let mut profile_id = vec![0; 16];
            profile_id[8..].copy_from_slice(
                &u64::try_from(profile_index + 1)
                    .expect("bounded fixture")
                    .to_be_bytes(),
            );
            Profile {
                sample_type: Some(ValueType {
                    type_strindex: sample_type_index,
                    unit_strindex: sample_unit_index,
                }),
                samples,
                time_unix_nano: 1_700_000_000_000_000_000
                    + u64::try_from(profile_index).expect("bounded fixture") * 1_000_000,
                duration_nano: 1_000_000,
                period_type: Some(ValueType {
                    type_strindex: period_type_index,
                    unit_strindex: sample_unit_index,
                }),
                period: 10,
                profile_id,
                dropped_attributes_count: 0,
                original_payload_format: if kind == ProfilesDatasetKind::OriginalPayload {
                    "pprof".to_string()
                } else {
                    String::new()
                },
                original_payload: if kind == ProfilesDatasetKind::OriginalPayload {
                    vec![0x50; 4096]
                } else {
                    Vec::new()
                },
                attribute_indices: vec![common_attribute],
            }
        })
        .collect();

    ProfilesData {
        resource_profiles: vec![ResourceProfiles {
            resource: Some(Resource {
                attributes: vec![KeyValue::new(
                    "service.name",
                    AnyValue::new_string("profiles-fixture"),
                )],
                ..Default::default()
            }),
            scope_profiles: vec![ScopeProfiles {
                scope: Some(InstrumentationScope {
                    name: "profiles.fixture".to_string(),
                    version: "1.0.0".to_string(),
                    attributes: vec![KeyValue::new(
                        "fixture.kind",
                        AnyValue::new_string(kind.as_str()),
                    )],
                    ..Default::default()
                }),
                profiles,
                schema_url: "https://opentelemetry.io/schemas/1.0.0".to_string(),
            }],
            schema_url: "https://opentelemetry.io/schemas/1.0.0".to_string(),
        }],
        dictionary: Some(dictionary.dictionary),
    }
}

/// Standard representative dataset set for validation and benchmarks.
#[must_use]
pub fn representative_profiles_datasets() -> Vec<(ProfilesDatasetKind, ProfilesData)> {
    [
        ProfilesDatasetKind::Cpu,
        ProfilesDatasetKind::Allocation,
        ProfilesDatasetKind::OffCpu,
        ProfilesDatasetKind::TimestampOnly,
        ProfilesDatasetKind::HighCardinalityAttributes,
        ProfilesDatasetKind::OriginalPayload,
    ]
    .into_iter()
    .map(|kind| (kind, profiles_dataset(kind, 4, 32, 16)))
    .collect()
}

struct FixtureDictionary {
    dictionary: ProfilesDictionary,
    strings: HashMap<String, i32>,
}

impl FixtureDictionary {
    fn new() -> Self {
        let dictionary = ProfilesDictionary {
            mapping_table: vec![Mapping::default()],
            location_table: vec![Location::default()],
            function_table: vec![Function::default()],
            link_table: vec![Link::default()],
            string_table: vec![String::new()],
            attribute_table: vec![KeyValueAndUnit::default()],
            stack_table: vec![Stack::default()],
        };
        let mut strings = HashMap::new();
        let _ = strings.insert(String::new(), 0);
        Self {
            dictionary,
            strings,
        }
    }

    fn intern_string(&mut self, value: &str) -> i32 {
        if let Some(index) = self.strings.get(value) {
            return *index;
        }
        let index = i32::try_from(self.dictionary.string_table.len())
            .expect("fixture string table remains bounded");
        self.dictionary.string_table.push(value.to_string());
        let _ = self.strings.insert(value.to_string(), index);
        index
    }

    fn push_attribute(&mut self, key: &str, value: AnyValue, unit: Option<&str>) -> i32 {
        let key_strindex = self.intern_string(key);
        let unit_strindex = unit
            .map(|unit| self.intern_string(unit))
            .unwrap_or_default();
        let index = i32::try_from(self.dictionary.attribute_table.len())
            .expect("fixture attribute table remains bounded");
        self.dictionary.attribute_table.push(KeyValueAndUnit {
            key_strindex,
            value: Some(value),
            unit_strindex,
        });
        index
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use rand::rngs::StdRng;
    use rand::{RngExt, SeedableRng};

    use super::*;
    use crate::TryIntoWithOptions;
    use crate::encode::encode_profiles_otap_batch;
    use crate::otap::{OtapArrowRecords, OtapBatchStore};
    use crate::otlp::OtlpProtoBytes;
    use crate::proto::OtlpProtoMessage;
    use crate::proto::opentelemetry::arrow::v1::ArrowPayloadType;
    use crate::testing::equiv::validate_equivalent;

    /// Scenario: Every representative generated Profiles workload completes a semantic round trip.
    /// Guarantees: CPU, allocation, off-CPU, timestamp-only, attributes, and originals are covered.
    #[test]
    fn representative_datasets_round_trip() {
        for (_, data) in representative_profiles_datasets() {
            let records = encode_profiles_otap_batch(&data).unwrap();
            let bytes: OtlpProtoBytes = crate::OtapPayload::from_otap(records)
                .try_into_with_default()
                .unwrap();
            let decoded: OtlpProtoMessage = bytes.try_into().unwrap();
            assert!(validate_equivalent(
                &[OtlpProtoMessage::Profiles(data)],
                &[decoded]
            ));
        }
    }

    /// Scenario: Bounded randomized Profiles graphs vary workload, size, and stack depth.
    /// Guarantees: Both conversion directions remain panic-free and semantically equivalent.
    #[test]
    fn bounded_randomized_profiles_round_trip() {
        let kinds = [
            ProfilesDatasetKind::Cpu,
            ProfilesDatasetKind::Allocation,
            ProfilesDatasetKind::OffCpu,
            ProfilesDatasetKind::TimestampOnly,
            ProfilesDatasetKind::HighCardinalityAttributes,
            ProfilesDatasetKind::OriginalPayload,
        ];
        for seed in 0..128_u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let kind = kinds[rng.random_range(0..kinds.len())];
            let data = profiles_dataset(
                kind,
                rng.random_range(1..=4),
                rng.random_range(1..=16),
                rng.random_range(1..=8),
            );
            let result = catch_unwind(AssertUnwindSafe(|| {
                let records = encode_profiles_otap_batch(&data)?;
                let bytes: OtlpProtoBytes = crate::OtapPayload::from_otap(records)
                    .try_into_with_default()
                    .map_err(crate::encode::Error::OtapError)?;
                let decoded: OtlpProtoMessage = bytes
                    .try_into()
                    .map_err(crate::encode::Error::ProtobufDecode)?;
                Ok::<_, crate::encode::Error>(decoded)
            }));
            let decoded = result.expect("Profiles conversion must not panic").unwrap();
            assert!(validate_equivalent(
                &[OtlpProtoMessage::Profiles(data)],
                &[decoded]
            ));
        }
    }

    /// Scenario: Random bounded byte strings are presented as OTLP Profiles requests.
    /// Guarantees: Protobuf decode and OTLP-to-OTAP conversion return normally without panicking.
    #[test]
    fn arbitrary_profiles_bytes_do_not_panic() {
        for seed in 0..256_u64 {
            let mut rng = StdRng::seed_from_u64(seed);
            let mut bytes = vec![0; rng.random_range(0..=512)];
            rng.fill(bytes.as_mut_slice());
            let result = catch_unwind(AssertUnwindSafe(|| {
                let result: Result<OtapArrowRecords, _> =
                    OtlpProtoBytes::ExportProfilesRequest(bytes.into()).try_into_with_default();
                result
            }));
            assert!(result.is_ok(), "seed {seed} panicked");
        }
    }

    /// Scenario: Required and optional payloads are removed from valid randomized OTAP graphs.
    /// Guarantees: OTAP-to-OTLP conversion succeeds or returns a typed error without panicking.
    #[test]
    fn mutated_profiles_graphs_do_not_panic() {
        let payload_types = [
            ArrowPayloadType::Profiles,
            ArrowPayloadType::Samples,
            ArrowPayloadType::Stacks,
            ArrowPayloadType::StackLocations,
            ArrowPayloadType::ProfileLocations,
            ArrowPayloadType::ProfileLocationLines,
            ArrowPayloadType::ProfileFunctions,
            ArrowPayloadType::ProfileMappings,
            ArrowPayloadType::ProfileLinks,
            ArrowPayloadType::ProfileAttrs,
        ];
        for (seed, payload_type) in payload_types.into_iter().cycle().take(128).enumerate() {
            let data = profiles_dataset(ProfilesDatasetKind::Cpu, 2, 4, 4);
            let records = encode_profiles_otap_batch(&data).unwrap();
            let OtapArrowRecords::Profiles(mut profiles) = records else {
                panic!("expected Profiles");
            };
            profiles.remove(payload_type);
            let result = catch_unwind(AssertUnwindSafe(|| {
                let payload = crate::OtapPayload::from_otap(OtapArrowRecords::Profiles(profiles));
                let result: Result<OtlpProtoBytes, _> = payload.try_into_with_default();
                result
            }));
            assert!(result.is_ok(), "mutation seed {seed} panicked");
        }
    }
}
