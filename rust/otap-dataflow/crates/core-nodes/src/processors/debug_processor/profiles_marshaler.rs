// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded human-readable formatting for OTLP Profiles.

use super::Verbosity;
use otel_arrow_dfe_pdata::proto::opentelemetry::common::v1::{AnyValue, KeyValue, any_value};
use otel_arrow_dfe_pdata::proto::opentelemetry::profiles::v1development::{
    KeyValueAndUnit, Profile, ProfilesData, ProfilesDictionary, Sample, ValueType,
};
use std::fmt::Write as _;

const MAX_PROFILES: usize = 8;
const MAX_SAMPLES_PER_PROFILE: usize = 8;
const MAX_FRAMES_PER_STACK: usize = 32;
const MAX_LINES_PER_LOCATION: usize = 4;
const MAX_ATTRIBUTES: usize = 16;
const MAX_OBSERVATIONS: usize = 16;
const MAX_STRING_CHARS: usize = 256;
const MAX_BYTES: usize = 16;

pub(super) fn marshal_profiles(data: &ProfilesData, verbosity: Verbosity) -> String {
    let resource_profiles = data.resource_profiles.len();
    let scope_profiles = data
        .resource_profiles
        .iter()
        .map(|resource| resource.scope_profiles.len())
        .sum::<usize>();
    let profiles = data
        .resource_profiles
        .iter()
        .flat_map(|resource| &resource.scope_profiles)
        .map(|scope| scope.profiles.len())
        .sum::<usize>();
    let samples = data
        .resource_profiles
        .iter()
        .flat_map(|resource| &resource.scope_profiles)
        .flat_map(|scope| &scope.profiles)
        .map(|profile| profile.samples.len())
        .sum::<usize>();

    let mut output = String::new();
    let _ = writeln!(output, "Received {resource_profiles} resource profiles");
    let _ = writeln!(output, "Received {scope_profiles} scope profiles");
    let _ = writeln!(output, "Received {profiles} profiles");
    let _ = writeln!(output, "Received {samples} samples");
    if verbosity == Verbosity::Basic {
        return output;
    }

    let dictionary = data.dictionary.as_ref();
    if let Some(dictionary) = dictionary {
        let _ = writeln!(
            output,
            "Dictionary: mappings={} locations={} functions={} links={} stacks={} attributes={}",
            dictionary.mapping_table.len().saturating_sub(1),
            dictionary.location_table.len().saturating_sub(1),
            dictionary.function_table.len().saturating_sub(1),
            dictionary.link_table.len().saturating_sub(1),
            dictionary.stack_table.len().saturating_sub(1),
            dictionary.attribute_table.len().saturating_sub(1),
        );
    } else {
        let _ = writeln!(output, "Dictionary: missing");
    }

    let mut emitted_profiles = 0_usize;
    for (resource_index, resource) in data.resource_profiles.iter().enumerate() {
        let _ = writeln!(output, "ResourceProfiles index={resource_index}");
        if let Some(resource) = resource.resource.as_ref() {
            format_key_values(
                &mut output,
                dictionary,
                "  resource.attribute",
                &resource.attributes,
            );
        }
        for (scope_index, scope) in resource.scope_profiles.iter().enumerate() {
            let _ = writeln!(output, "  ScopeProfiles index={scope_index}");
            if let Some(instrumentation_scope) = scope.scope.as_ref() {
                let _ = writeln!(
                    output,
                    "    scope.name={} scope.version={}",
                    limited_string(&instrumentation_scope.name),
                    limited_string(&instrumentation_scope.version),
                );
                format_key_values(
                    &mut output,
                    dictionary,
                    "    scope.attribute",
                    &instrumentation_scope.attributes,
                );
            }
            for (profile_index, profile) in scope.profiles.iter().enumerate() {
                if emitted_profiles >= MAX_PROFILES {
                    let _ = writeln!(
                        output,
                        "... omitted {} profiles",
                        profiles.saturating_sub(emitted_profiles)
                    );
                    return output;
                }
                format_profile(
                    &mut output,
                    dictionary,
                    profile,
                    resource_index,
                    scope_index,
                    profile_index,
                    verbosity,
                );
                emitted_profiles += 1;
            }
        }
    }
    output
}

fn format_profile(
    output: &mut String,
    dictionary: Option<&ProfilesDictionary>,
    profile: &Profile,
    resource_index: usize,
    scope_index: usize,
    profile_index: usize,
    verbosity: Verbosity,
) {
    let _ = writeln!(
        output,
        "Profile resource={resource_index} scope={scope_index} index={profile_index} id={} time_unix_nano={} duration_nano={} samples={}",
        hex_bytes(&profile.profile_id, MAX_BYTES),
        profile.time_unix_nano,
        profile.duration_nano,
        profile.samples.len(),
    );
    if let Some(sample_type) = profile.sample_type.as_ref() {
        let _ = writeln!(
            output,
            "  sample_type={}",
            format_value_type(dictionary, sample_type)
        );
    }
    if let Some(period_type) = profile.period_type.as_ref() {
        let _ = writeln!(
            output,
            "  period={} period_type={}",
            profile.period,
            format_value_type(dictionary, period_type)
        );
    }
    format_attributes(
        output,
        dictionary,
        "  attributes",
        &profile.attribute_indices,
    );
    if verbosity != Verbosity::Detailed {
        return;
    }

    for (sample_index, sample) in profile
        .samples
        .iter()
        .take(MAX_SAMPLES_PER_PROFILE)
        .enumerate()
    {
        format_sample(output, dictionary, sample, sample_index);
    }
    if profile.samples.len() > MAX_SAMPLES_PER_PROFILE {
        let _ = writeln!(
            output,
            "  ... omitted {} samples",
            profile.samples.len() - MAX_SAMPLES_PER_PROFILE
        );
    }
}

fn format_sample(
    output: &mut String,
    dictionary: Option<&ProfilesDictionary>,
    sample: &Sample,
    sample_index: usize,
) {
    let _ = writeln!(
        output,
        "  Sample index={sample_index} stack_index={} link_index={} values={} timestamps_unix_nano={}",
        sample.stack_index,
        sample.link_index,
        limited_slice(&sample.values),
        limited_slice(&sample.timestamps_unix_nano),
    );
    format_attributes(
        output,
        dictionary,
        "    attributes",
        &sample.attribute_indices,
    );

    let Some(dictionary) = dictionary else {
        return;
    };
    let Some(stack) = table_entry(&dictionary.stack_table, sample.stack_index) else {
        let _ = writeln!(output, "    stack: unresolved index {}", sample.stack_index);
        return;
    };
    for (frame_index, location_index) in stack
        .location_indices
        .iter()
        .take(MAX_FRAMES_PER_STACK)
        .enumerate()
    {
        let Some(location) = table_entry(&dictionary.location_table, *location_index) else {
            let _ = writeln!(
                output,
                "    Frame index={frame_index} unresolved location_index={location_index}"
            );
            continue;
        };
        let _ = writeln!(
            output,
            "    Frame index={frame_index} location_index={location_index} address=0x{:x} mapping_index={}",
            location.address, location.mapping_index,
        );
        for line in location.lines.iter().take(MAX_LINES_PER_LOCATION) {
            let Some(function) = table_entry(&dictionary.function_table, line.function_index)
            else {
                let _ = writeln!(
                    output,
                    "      unresolved function_index={} line={} column={}",
                    line.function_index, line.line, line.column
                );
                continue;
            };
            let _ = writeln!(
                output,
                "      function={} file={} line={} column={}",
                dictionary_string(dictionary, function.name_strindex),
                dictionary_string(dictionary, function.filename_strindex),
                line.line,
                line.column,
            );
        }
        if location.lines.len() > MAX_LINES_PER_LOCATION {
            let _ = writeln!(
                output,
                "      ... omitted {} lines",
                location.lines.len() - MAX_LINES_PER_LOCATION
            );
        }
    }
    if stack.location_indices.len() > MAX_FRAMES_PER_STACK {
        let _ = writeln!(
            output,
            "    ... omitted {} frames",
            stack.location_indices.len() - MAX_FRAMES_PER_STACK
        );
    }
}

fn format_attributes(
    output: &mut String,
    dictionary: Option<&ProfilesDictionary>,
    label: &str,
    indices: &[i32],
) {
    let Some(dictionary) = dictionary else {
        if !indices.is_empty() {
            let _ = writeln!(output, "{label}: unresolved without dictionary");
        }
        return;
    };
    for index in indices.iter().take(MAX_ATTRIBUTES) {
        let Some(attribute) = table_entry(&dictionary.attribute_table, *index) else {
            let _ = writeln!(output, "{label}: unresolved index {index}");
            continue;
        };
        let _ = writeln!(
            output,
            "{label}: {}={}{}",
            dictionary_string(dictionary, attribute.key_strindex),
            format_attribute_value(dictionary, attribute),
            format_unit(dictionary, attribute),
        );
    }
    if indices.len() > MAX_ATTRIBUTES {
        let _ = writeln!(
            output,
            "{label}: ... omitted {} attributes",
            indices.len() - MAX_ATTRIBUTES
        );
    }
}

fn format_value_type(dictionary: Option<&ProfilesDictionary>, value_type: &ValueType) -> String {
    let Some(dictionary) = dictionary else {
        return "<missing dictionary>".to_string();
    };
    format!(
        "{} ({})",
        dictionary_string(dictionary, value_type.type_strindex),
        dictionary_string(dictionary, value_type.unit_strindex)
    )
}

fn format_attribute_value(dictionary: &ProfilesDictionary, attribute: &KeyValueAndUnit) -> String {
    format_any_value(dictionary, attribute.value.as_ref())
}

fn format_key_values(
    output: &mut String,
    dictionary: Option<&ProfilesDictionary>,
    label: &str,
    attributes: &[KeyValue],
) {
    for attribute in attributes.iter().take(MAX_ATTRIBUTES) {
        let key = if attribute.key.is_empty() {
            dictionary.map_or_else(
                || format!("<index:{}>", attribute.key_strindex),
                |dictionary| dictionary_string(dictionary, attribute.key_strindex),
            )
        } else {
            limited_string(&attribute.key)
        };
        let value = dictionary.map_or_else(
            || "<missing dictionary>".to_string(),
            |dictionary| format_any_value(dictionary, attribute.value.as_ref()),
        );
        let _ = writeln!(output, "{label}: {key}={value}");
    }
    if attributes.len() > MAX_ATTRIBUTES {
        let _ = writeln!(
            output,
            "{label}: ... omitted {} attributes",
            attributes.len() - MAX_ATTRIBUTES
        );
    }
}

fn format_any_value(dictionary: &ProfilesDictionary, value: Option<&AnyValue>) -> String {
    match value.and_then(|value| value.value.as_ref()) {
        None => "<empty>".to_string(),
        Some(any_value::Value::StringValue(value)) => limited_string(value),
        Some(any_value::Value::StringValueStrindex(index)) => dictionary_string(dictionary, *index),
        Some(any_value::Value::BoolValue(value)) => value.to_string(),
        Some(any_value::Value::IntValue(value)) => value.to_string(),
        Some(any_value::Value::DoubleValue(value)) => value.to_string(),
        Some(any_value::Value::BytesValue(value)) => format!("0x{}", hex_bytes(value, MAX_BYTES)),
        Some(any_value::Value::ArrayValue(value)) => format!("array[len={}]", value.values.len()),
        Some(any_value::Value::KvlistValue(value)) => format!("kvlist[len={}]", value.values.len()),
    }
}

fn format_unit(dictionary: &ProfilesDictionary, attribute: &KeyValueAndUnit) -> String {
    if attribute.unit_strindex == 0 {
        String::new()
    } else {
        format!(
            " unit={}",
            dictionary_string(dictionary, attribute.unit_strindex)
        )
    }
}

fn dictionary_string(dictionary: &ProfilesDictionary, index: i32) -> String {
    let Ok(index) = usize::try_from(index) else {
        return format!("<invalid:{index}>");
    };
    dictionary.string_table.get(index).map_or_else(
        || format!("<missing:{index}>"),
        |value| limited_string(value),
    )
}

fn table_entry<T>(table: &[T], index: i32) -> Option<&T> {
    usize::try_from(index)
        .ok()
        .and_then(|index| table.get(index))
}

fn limited_string(value: &str) -> String {
    let mut chars = value.chars();
    let mut result = String::new();
    for character in chars.by_ref().take(MAX_STRING_CHARS) {
        result.extend(character.escape_default());
    }
    if chars.next().is_some() {
        result.push_str("...");
    }
    result
}

fn hex_bytes(value: &[u8], limit: usize) -> String {
    let mut result = String::new();
    for byte in value.iter().take(limit) {
        let _ = write!(result, "{byte:02x}");
    }
    if value.len() > limit {
        result.push_str("...");
    }
    result
}

fn limited_slice<T: std::fmt::Debug>(values: &[T]) -> String {
    let mut result = format!("{:?}", &values[..values.len().min(MAX_OBSERVATIONS)]);
    if values.len() > MAX_OBSERVATIONS {
        let _ = write!(result, " (... {} omitted)", values.len() - MAX_OBSERVATIONS);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use otel_arrow_dfe_pdata::testing::profiles::{ProfilesDatasetKind, profiles_dataset};

    /// Scenario: Basic Profiles debug output receives a shared graph.
    /// Guarantees: Root and sample totals are reported without dumping graph details.
    #[test]
    fn basic_output_reports_counts() {
        let data = profiles_dataset(ProfilesDatasetKind::Cpu, 2, 3, 4);
        let output = marshal_profiles(&data, Verbosity::Basic);
        assert!(output.contains("Received 2 profiles"));
        assert!(output.contains("Received 6 samples"));
        assert!(!output.contains("Dictionary:"));
    }

    /// Scenario: Detailed Profiles output contains symbolized shared stack information.
    /// Guarantees: Profile attributes, samples, function names, and filenames are inspectable.
    #[test]
    fn detailed_output_reports_bounded_graph_details() {
        let data = profiles_dataset(ProfilesDatasetKind::Cpu, 1, 2, 2);
        let output = marshal_profiles(&data, Verbosity::Detailed);
        assert!(output.contains("Dictionary:"));
        assert!(output.contains("resource.attribute: service.name=profiles-fixture"));
        assert!(output.contains("scope.name=profiles.fixture"));
        assert!(output.contains("profile.kind=cpu"));
        assert!(output.contains("Sample index=0"));
        assert!(output.contains("function=fixture_function_0"));
        assert!(output.contains("file=fixture_0.rs"));
    }

    /// Scenario: Profile metadata contains control characters.
    /// Guarantees: Readable output escapes control characters instead of injecting new lines.
    #[test]
    fn strings_escape_control_characters() {
        assert_eq!(limited_string("worker\nname"), "worker\\nname");
    }
}
