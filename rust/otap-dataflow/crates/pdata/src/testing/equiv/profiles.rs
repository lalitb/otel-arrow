// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Profiles equivalence checking over the reachable logical graph.

use crate::encode::encode_profiles_otap_batch;
use crate::proto::opentelemetry::common::v1::{AnyValue, KeyValue, any_value};
use crate::proto::opentelemetry::profiles::v1development::{
    Function, KeyValueAndUnit, Line, Link, Location, Mapping, Profile, ProfilesData,
    ProfilesDictionary, Sample, Stack, ValueType,
};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum CanonicalAnyValue {
    Empty,
    String(String),
    Bool(bool),
    Int(i64),
    Double(u64),
    Bytes(Vec<u8>),
    Array(Vec<CanonicalAnyValue>),
    KeyValueList(Vec<(String, CanonicalAnyValue)>),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CanonicalAttribute {
    key: String,
    value: CanonicalAnyValue,
    unit: Option<String>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CanonicalMapping {
    memory_start: u64,
    memory_limit: u64,
    file_offset: u64,
    filename: Option<String>,
    attributes: Vec<CanonicalAttribute>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CanonicalFunction {
    name: Option<String>,
    system_name: Option<String>,
    filename: Option<String>,
    start_line: i64,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CanonicalLine {
    function: Option<CanonicalFunction>,
    line: i64,
    column: i64,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CanonicalLocation {
    mapping: Option<CanonicalMapping>,
    address: u64,
    lines: Vec<CanonicalLine>,
    attributes: Vec<CanonicalAttribute>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CanonicalStack {
    locations: Vec<Option<CanonicalLocation>>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CanonicalLink {
    trace_id: Vec<u8>,
    span_id: Vec<u8>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CanonicalValueType {
    r#type: String,
    unit: String,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CanonicalSample {
    stack: Option<CanonicalStack>,
    attributes: Vec<CanonicalAttribute>,
    link: Option<CanonicalLink>,
    values: Vec<i64>,
    timestamps_unix_nano: Vec<u64>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct CanonicalProfile {
    resource_schema_url: String,
    resource_attributes: Vec<CanonicalAttribute>,
    resource_dropped_attributes_count: u32,
    scope_name: String,
    scope_version: String,
    scope_attributes: Vec<CanonicalAttribute>,
    scope_dropped_attributes_count: u32,
    scope_schema_url: String,
    sample_type: Option<CanonicalValueType>,
    samples: Vec<CanonicalSample>,
    time_unix_nano: u64,
    duration_nano: u64,
    period_type: Option<CanonicalValueType>,
    period: i64,
    profile_id: Vec<u8>,
    dropped_attributes_count: u32,
    original_payload_format: String,
    original_payload: Vec<u8>,
    attributes: Vec<CanonicalAttribute>,
}

fn canonicalize(messages: &[ProfilesData]) -> Option<Vec<CanonicalProfile>> {
    let mut result = Vec::new();
    for data in messages {
        _ = encode_profiles_otap_batch(data).ok()?;
        let dictionary = data.dictionary.as_ref();
        for resource_profiles in &data.resource_profiles {
            if resource_profiles
                .resource
                .as_ref()
                .is_some_and(|resource| !resource.entity_refs.is_empty())
            {
                return None;
            }
            let resource_attributes = canonical_direct_attributes(
                resource_profiles
                    .resource
                    .as_ref()
                    .map(|resource| resource.attributes.as_slice())
                    .unwrap_or_default(),
                dictionary,
            )?;
            let resource_dropped_attributes_count = resource_profiles
                .resource
                .as_ref()
                .map_or(0, |resource| resource.dropped_attributes_count);
            for scope_profiles in &resource_profiles.scope_profiles {
                let scope_attributes = canonical_direct_attributes(
                    scope_profiles
                        .scope
                        .as_ref()
                        .map(|scope| scope.attributes.as_slice())
                        .unwrap_or_default(),
                    dictionary,
                )?;
                for profile in &scope_profiles.profiles {
                    result.push(canonical_profile(
                        profile,
                        dictionary?,
                        resource_profiles.schema_url.clone(),
                        resource_attributes.clone(),
                        resource_dropped_attributes_count,
                        scope_profiles
                            .scope
                            .as_ref()
                            .map(|scope| scope.name.clone())
                            .unwrap_or_default(),
                        scope_profiles
                            .scope
                            .as_ref()
                            .map(|scope| scope.version.clone())
                            .unwrap_or_default(),
                        scope_attributes.clone(),
                        scope_profiles
                            .scope
                            .as_ref()
                            .map_or(0, |scope| scope.dropped_attributes_count),
                        scope_profiles.schema_url.clone(),
                    )?);
                }
            }
        }
    }
    result.sort();
    Some(result)
}

#[allow(clippy::too_many_arguments)]
fn canonical_profile(
    profile: &Profile,
    dictionary: &ProfilesDictionary,
    resource_schema_url: String,
    resource_attributes: Vec<CanonicalAttribute>,
    resource_dropped_attributes_count: u32,
    scope_name: String,
    scope_version: String,
    scope_attributes: Vec<CanonicalAttribute>,
    scope_dropped_attributes_count: u32,
    scope_schema_url: String,
) -> Option<CanonicalProfile> {
    let mut samples = profile
        .samples
        .iter()
        .map(|sample| canonical_sample(sample, dictionary))
        .collect::<Option<Vec<_>>>()?;
    samples.sort();
    Some(CanonicalProfile {
        resource_schema_url,
        resource_attributes,
        resource_dropped_attributes_count,
        scope_name,
        scope_version,
        scope_attributes,
        scope_dropped_attributes_count,
        scope_schema_url,
        sample_type: canonical_value_type(profile.sample_type.as_ref(), dictionary)?,
        samples,
        time_unix_nano: profile.time_unix_nano,
        duration_nano: profile.duration_nano,
        period_type: canonical_value_type(profile.period_type.as_ref(), dictionary)?,
        period: profile.period,
        profile_id: profile.profile_id.clone(),
        dropped_attributes_count: profile.dropped_attributes_count,
        original_payload_format: profile.original_payload_format.clone(),
        original_payload: profile.original_payload.clone(),
        attributes: canonical_referenced_attributes(&profile.attribute_indices, dictionary)?,
    })
}

fn canonical_sample(sample: &Sample, dictionary: &ProfilesDictionary) -> Option<CanonicalSample> {
    Some(CanonicalSample {
        stack: canonical_reference(&dictionary.stack_table, sample.stack_index, |stack| {
            canonical_stack(stack, dictionary)
        })?,
        attributes: canonical_referenced_attributes(&sample.attribute_indices, dictionary)?,
        link: canonical_reference(&dictionary.link_table, sample.link_index, canonical_link)?,
        values: sample.values.clone(),
        timestamps_unix_nano: sample.timestamps_unix_nano.clone(),
    })
}

fn canonical_stack(stack: &Stack, dictionary: &ProfilesDictionary) -> Option<CanonicalStack> {
    Some(CanonicalStack {
        locations: stack
            .location_indices
            .iter()
            .map(|index| {
                canonical_reference(&dictionary.location_table, *index, |location| {
                    canonical_location(location, dictionary)
                })
            })
            .collect::<Option<Vec<_>>>()?,
    })
}

fn canonical_location(
    location: &Location,
    dictionary: &ProfilesDictionary,
) -> Option<CanonicalLocation> {
    Some(CanonicalLocation {
        mapping: canonical_reference(
            &dictionary.mapping_table,
            location.mapping_index,
            |mapping| canonical_mapping(mapping, dictionary),
        )?,
        address: location.address,
        lines: location
            .lines
            .iter()
            .map(|line| canonical_line(line, dictionary))
            .collect::<Option<Vec<_>>>()?,
        attributes: canonical_referenced_attributes(&location.attribute_indices, dictionary)?,
    })
}

fn canonical_mapping(
    mapping: &Mapping,
    dictionary: &ProfilesDictionary,
) -> Option<CanonicalMapping> {
    Some(CanonicalMapping {
        memory_start: mapping.memory_start,
        memory_limit: mapping.memory_limit,
        file_offset: mapping.file_offset,
        filename: canonical_optional_string(dictionary, mapping.filename_strindex)?,
        attributes: canonical_referenced_attributes(&mapping.attribute_indices, dictionary)?,
    })
}

fn canonical_line(line: &Line, dictionary: &ProfilesDictionary) -> Option<CanonicalLine> {
    Some(CanonicalLine {
        function: canonical_reference(
            &dictionary.function_table,
            line.function_index,
            |function| canonical_function(function, dictionary),
        )?,
        line: line.line,
        column: line.column,
    })
}

fn canonical_function(
    function: &Function,
    dictionary: &ProfilesDictionary,
) -> Option<CanonicalFunction> {
    Some(CanonicalFunction {
        name: canonical_optional_string(dictionary, function.name_strindex)?,
        system_name: canonical_optional_string(dictionary, function.system_name_strindex)?,
        filename: canonical_optional_string(dictionary, function.filename_strindex)?,
        start_line: function.start_line,
    })
}

fn canonical_link(link: &Link) -> Option<CanonicalLink> {
    Some(CanonicalLink {
        trace_id: link.trace_id.clone(),
        span_id: link.span_id.clone(),
    })
}

fn canonical_value_type(
    value_type: Option<&ValueType>,
    dictionary: &ProfilesDictionary,
) -> Option<Option<CanonicalValueType>> {
    let Some(value_type) = value_type else {
        return Some(None);
    };
    let r#type = canonical_string(dictionary, value_type.type_strindex)?.to_string();
    let unit = canonical_string(dictionary, value_type.unit_strindex)?.to_string();
    Some((!r#type.is_empty() || !unit.is_empty()).then_some(CanonicalValueType { r#type, unit }))
}

fn canonical_referenced_attributes(
    indices: &[i32],
    dictionary: &ProfilesDictionary,
) -> Option<Vec<CanonicalAttribute>> {
    let mut result = indices
        .iter()
        .filter_map(|index| canonical_index(&dictionary.attribute_table, *index))
        .map(|attribute| canonical_attribute(attribute, dictionary))
        .collect::<Option<Vec<_>>>()?;
    result.sort();
    Some(result)
}

fn canonical_direct_attributes(
    attributes: &[KeyValue],
    dictionary: Option<&ProfilesDictionary>,
) -> Option<Vec<CanonicalAttribute>> {
    let mut result = attributes
        .iter()
        .map(|attribute| {
            let dictionary = dictionary?;
            let key = if attribute.key.is_empty() {
                canonical_string(dictionary, attribute.key_strindex)?.to_string()
            } else if attribute.key_strindex == 0 {
                attribute.key.clone()
            } else {
                return None;
            };
            Some(CanonicalAttribute {
                key,
                value: canonical_any_value(attribute.value.as_ref(), dictionary)?,
                unit: None,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    result.sort();
    Some(result)
}

fn canonical_attribute(
    attribute: &KeyValueAndUnit,
    dictionary: &ProfilesDictionary,
) -> Option<CanonicalAttribute> {
    Some(CanonicalAttribute {
        key: canonical_string(dictionary, attribute.key_strindex)?.to_string(),
        value: canonical_any_value(attribute.value.as_ref(), dictionary)?,
        unit: canonical_optional_string(dictionary, attribute.unit_strindex)?,
    })
}

fn canonical_any_value(
    value: Option<&AnyValue>,
    dictionary: &ProfilesDictionary,
) -> Option<CanonicalAnyValue> {
    Some(match value.and_then(|value| value.value.as_ref()) {
        None => CanonicalAnyValue::Empty,
        Some(any_value::Value::StringValue(value)) => CanonicalAnyValue::String(value.clone()),
        Some(any_value::Value::StringValueStrindex(index)) => {
            CanonicalAnyValue::String(canonical_string(dictionary, *index)?.to_string())
        }
        Some(any_value::Value::BoolValue(value)) => CanonicalAnyValue::Bool(*value),
        Some(any_value::Value::IntValue(value)) => CanonicalAnyValue::Int(*value),
        Some(any_value::Value::DoubleValue(value)) => CanonicalAnyValue::Double(value.to_bits()),
        Some(any_value::Value::BytesValue(value)) => CanonicalAnyValue::Bytes(value.clone()),
        Some(any_value::Value::ArrayValue(value)) => CanonicalAnyValue::Array(
            value
                .values
                .iter()
                .map(|value| canonical_any_value(Some(value), dictionary))
                .collect::<Option<Vec<_>>>()?,
        ),
        Some(any_value::Value::KvlistValue(value)) => {
            let mut values = value
                .values
                .iter()
                .map(|attribute| {
                    let key = if attribute.key.is_empty() {
                        canonical_string(dictionary, attribute.key_strindex)?.to_string()
                    } else if attribute.key_strindex == 0 {
                        attribute.key.clone()
                    } else {
                        return None;
                    };
                    Some((
                        key,
                        canonical_any_value(attribute.value.as_ref(), dictionary)?,
                    ))
                })
                .collect::<Option<Vec<_>>>()?;
            values.sort();
            CanonicalAnyValue::KeyValueList(values)
        }
    })
}

fn canonical_optional_string(
    dictionary: &ProfilesDictionary,
    index: i32,
) -> Option<Option<String>> {
    let value = canonical_string(dictionary, index)?;
    Some((!value.is_empty()).then(|| value.to_string()))
}

fn canonical_string(dictionary: &ProfilesDictionary, index: i32) -> Option<&str> {
    dictionary
        .string_table
        .get(usize::try_from(index).ok()?)
        .map(String::as_str)
}

fn canonical_index<T>(table: &[T], index: i32) -> Option<&T> {
    let index = usize::try_from(index).ok()?;
    (index != 0).then(|| table.get(index)).flatten()
}

fn canonical_reference<T, U>(
    table: &[T],
    index: i32,
    canonicalize: impl FnOnce(&T) -> Option<U>,
) -> Option<Option<U>> {
    let index = usize::try_from(index).ok()?;
    if index == 0 {
        return Some(None);
    }
    Some(Some(canonicalize(table.get(index)?)?))
}

pub(super) fn validate_profiles_equivalent(left: &[ProfilesData], right: &[ProfilesData]) -> bool {
    match (canonicalize(left), canonicalize(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

pub(super) fn assert_profiles_equivalent(left: &[ProfilesData], right: &[ProfilesData]) {
    let left = canonicalize(left).expect("left Profiles data should canonicalize");
    let right = canonicalize(right).expect("right Profiles data should canonicalize");
    assert_eq!(
        left, right,
        "Profiles payloads are not semantically equivalent"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::opentelemetry::profiles::v1development::{ResourceProfiles, ScopeProfiles};

    fn profiles_with_function_order(reordered: bool) -> ProfilesData {
        let (string_table, function_table, function_index) = if reordered {
            (
                vec![String::new(), "orphan".to_string(), "target".to_string()],
                vec![
                    Function::default(),
                    Function {
                        name_strindex: 1,
                        ..Default::default()
                    },
                    Function {
                        name_strindex: 2,
                        ..Default::default()
                    },
                ],
                2,
            )
        } else {
            (
                vec![String::new(), "target".to_string(), "orphan".to_string()],
                vec![
                    Function::default(),
                    Function {
                        name_strindex: 1,
                        ..Default::default()
                    },
                    Function {
                        name_strindex: 2,
                        ..Default::default()
                    },
                ],
                1,
            )
        };
        ProfilesData {
            resource_profiles: vec![ResourceProfiles {
                scope_profiles: vec![ScopeProfiles {
                    profiles: vec![Profile {
                        samples: vec![Sample {
                            stack_index: 1,
                            values: vec![7],
                            ..Default::default()
                        }],
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            dictionary: Some(ProfilesDictionary {
                mapping_table: vec![Mapping::default()],
                location_table: vec![
                    Location::default(),
                    Location {
                        address: 1,
                        lines: vec![Line {
                            function_index,
                            line: 1,
                            column: 1,
                        }],
                        ..Default::default()
                    },
                ],
                function_table,
                link_table: vec![Link::default()],
                string_table,
                attribute_table: vec![KeyValueAndUnit::default()],
                stack_table: vec![
                    Stack::default(),
                    Stack {
                        location_indices: vec![1],
                    },
                ],
            }),
        }
    }

    /// Scenario: Equivalent Profiles messages reorder dictionary entries and retain an orphan.
    /// Guarantees: Semantic comparison follows reachable values instead of dictionary indexes.
    #[test]
    fn reordered_and_orphaned_dictionaries_are_equivalent() {
        let left = profiles_with_function_order(false);
        let right = profiles_with_function_order(true);

        assert!(validate_profiles_equivalent(&[left], &[right]));
    }

    /// Scenario: Two Profiles messages differ in one sample observation.
    /// Guarantees: Semantic comparison detects an observable profile value change.
    #[test]
    fn different_sample_values_are_not_equivalent() {
        let left = profiles_with_function_order(false);
        let mut right = profiles_with_function_order(true);
        right.resource_profiles[0].scope_profiles[0].profiles[0].samples[0].values[0] = 8;

        assert!(!validate_profiles_equivalent(&[left], &[right]));
    }
}
