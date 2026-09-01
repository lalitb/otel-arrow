// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! OTLP Profiles to OTAP Profiles encoding.

use std::collections::HashSet;
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, FixedSizeBinaryArray, Int64Array, LargeBinaryArray, LargeListArray,
    RecordBatch, UInt8Array, UInt32Array, UInt64Array,
};
use arrow::buffer::{OffsetBuffer, ScalarBuffer};
use arrow::datatypes::{DataType, Field, Schema};

use crate::encode::cbor::{serialize_any_values, serialize_kv_list};
use crate::encode::record::array::{
    ArrayAppendNulls, ArrayAppendSlice, ArrayOptions, BinaryArrayBuilder, binary_to_utf8_array,
    dictionary::DictionaryOptions,
};
use crate::encode::record::attributes::{AnyValuesRecordsBuilder, AttributesRecordBatchBuilder};
use crate::encode::record::logs::{ResourceBuilder, ScopeBuilder};
use crate::otap::{OtapArrowRecords, OtapBatchStore, Profiles};
use crate::proto::opentelemetry::arrow::v1::ArrowPayloadType;
use crate::proto::opentelemetry::collector::profiles::v1development::ExportProfilesServiceRequest;
use crate::proto::opentelemetry::common::v1::{AnyValue, KeyValue, any_value};
use crate::proto::opentelemetry::profiles::v1development::{
    Function, KeyValueAndUnit, Link, Location, Mapping, ProfilesData, ProfilesDictionary,
    ResourceProfiles, Stack, ValueType,
};
use crate::schema::{consts, update_field_metadata};
use crate::views::otlp::proto::common::{ObjAny, ObjKeyValue};
use crate::views::otlp::proto::wrappers::Wraps;

use super::{Error, Result};

const MAX_ANY_VALUE_DEPTH: usize = 64;

/// Encode a storage-form OTLP Profiles message into OTAP record batches.
pub fn encode_profiles_otap_batch(data: &ProfilesData) -> Result<OtapArrowRecords> {
    encode_profiles_parts(&data.resource_profiles, data.dictionary.as_ref())
}

/// Encode an OTLP Profiles export request into OTAP record batches.
pub fn encode_profiles_request_otap_batch(
    request: &ExportProfilesServiceRequest,
) -> Result<OtapArrowRecords> {
    encode_profiles_parts(&request.resource_profiles, request.dictionary.as_ref())
}

fn encode_profiles_parts(
    resource_profiles: &[ResourceProfiles],
    dictionary: Option<&ProfilesDictionary>,
) -> Result<OtapArrowRecords> {
    if !resource_profiles
        .iter()
        .flat_map(|resource| &resource.scope_profiles)
        .any(|scope| !scope.profiles.is_empty())
    {
        return Ok(OtapArrowRecords::Profiles(Profiles::default()));
    }

    let dictionary = dictionary.ok_or_else(|| invalid("missing ProfilesDictionary"))?;
    let resolver = DictionaryResolver::new(dictionary)?;

    let mut resource_attrs = AttributesRecordBatchBuilder::<u16>::new();
    let mut scope_attrs = AttributesRecordBatchBuilder::<u16>::new();
    let mut profile_attrs = ProfileAttributesBuilder::default();
    let mut sample_attrs = ProfileAttributesBuilder::default();
    let mut mapping_attrs = ProfileAttributesBuilder::default();
    let mut location_attrs = ProfileAttributesBuilder::default();

    let mappings = build_mappings(&resolver, &mut mapping_attrs)?;
    let functions = build_functions(&resolver)?;
    let links = build_links(&resolver)?;
    let (locations, location_lines) = build_locations(&resolver, &mut location_attrs)?;
    let (stacks, stack_locations) = build_stacks(&resolver)?;

    let mut roots = ProfilesRootBuilder::default();
    let mut value_types = ValueTypesBuilder::default();
    let mut samples = SamplesBuilder::default();
    let mut next_profile_id = 1_u32;
    let mut next_sample_id = 1_u32;
    let mut next_resource_id = 0_usize;
    let mut next_scope_id = 0_usize;

    for resource_profiles in resource_profiles {
        if !resource_profiles
            .scope_profiles
            .iter()
            .any(|scope| !scope.profiles.is_empty())
        {
            continue;
        }

        let resource_id = u16::try_from(next_resource_id).map_err(|_| Error::U16OverflowError)?;
        next_resource_id = next_resource_id
            .checked_add(1)
            .ok_or(Error::U16OverflowError)?;

        if let Some(resource) = &resource_profiles.resource {
            if !resource.entity_refs.is_empty() {
                return Err(invalid(
                    "Resource.entity_refs are not representable in the Profiles OTAP schema",
                ));
            }
            append_direct_attributes(
                &mut resource_attrs,
                resource_id,
                &resource.attributes,
                &resolver,
                "resource",
            )?;
        }

        for scope_profiles in &resource_profiles.scope_profiles {
            if scope_profiles.profiles.is_empty() {
                continue;
            }

            let scope_id = u16::try_from(next_scope_id).map_err(|_| Error::U16OverflowError)?;
            next_scope_id = next_scope_id
                .checked_add(1)
                .ok_or(Error::U16OverflowError)?;

            if let Some(scope) = &scope_profiles.scope {
                append_direct_attributes(
                    &mut scope_attrs,
                    scope_id,
                    &scope.attributes,
                    &resolver,
                    "scope",
                )?;
            }

            for profile in &scope_profiles.profiles {
                let profile_id = take_next_id(&mut next_profile_id)?;
                roots.append(
                    profile_id,
                    resource_id,
                    resource_profiles,
                    scope_id,
                    scope_profiles,
                    profile,
                )?;

                if let Some(value_type) = &profile.sample_type {
                    append_value_type(&mut value_types, &resolver, profile_id, 0, value_type)?;
                }
                if let Some(value_type) = &profile.period_type {
                    append_value_type(&mut value_types, &resolver, profile_id, 1, value_type)?;
                }

                _ = append_referenced_attributes(
                    &mut profile_attrs,
                    profile_id,
                    &profile.attribute_indices,
                    &resolver,
                    "profile",
                )?;

                for sample in &profile.samples {
                    let sample_id = take_next_id(&mut next_sample_id)?;
                    samples.append(sample_id, profile_id, sample, &resolver)?;
                    _ = append_referenced_attributes(
                        &mut sample_attrs,
                        sample_id,
                        &sample.attribute_indices,
                        &resolver,
                        "sample",
                    )?;
                }
            }
        }
    }

    let mut profiles = Profiles::default();
    profiles.set(ArrowPayloadType::Profiles, roots.finish()?)?;
    profiles.set(
        ArrowPayloadType::Samples,
        mark_parent_id_plain(samples.finish()?),
    )?;

    set_nonempty(
        &mut profiles,
        ArrowPayloadType::ProfileValueTypes,
        value_types.finish()?,
    )?;
    set_nonempty(&mut profiles, ArrowPayloadType::Stacks, stacks)?;
    set_nonempty(
        &mut profiles,
        ArrowPayloadType::StackLocations,
        stack_locations,
    )?;
    set_nonempty(&mut profiles, ArrowPayloadType::ProfileLocations, locations)?;
    set_nonempty(
        &mut profiles,
        ArrowPayloadType::ProfileLocationLines,
        location_lines,
    )?;
    set_nonempty(&mut profiles, ArrowPayloadType::ProfileFunctions, functions)?;
    set_nonempty(&mut profiles, ArrowPayloadType::ProfileMappings, mappings)?;
    set_nonempty(&mut profiles, ArrowPayloadType::ProfileLinks, links)?;
    set_nonempty(
        &mut profiles,
        ArrowPayloadType::ProfileAttrs,
        profile_attrs.finish()?,
    )?;
    set_nonempty(
        &mut profiles,
        ArrowPayloadType::ProfileSampleAttrs,
        sample_attrs.finish()?,
    )?;
    set_nonempty(
        &mut profiles,
        ArrowPayloadType::ProfileMappingAttrs,
        mapping_attrs.finish()?,
    )?;
    set_nonempty(
        &mut profiles,
        ArrowPayloadType::ProfileLocationAttrs,
        location_attrs.finish()?,
    )?;
    set_nonempty(
        &mut profiles,
        ArrowPayloadType::ResourceAttrs,
        resource_attrs.finish()?,
    )?;
    set_nonempty(
        &mut profiles,
        ArrowPayloadType::ScopeAttrs,
        scope_attrs.finish()?,
    )?;

    Ok(OtapArrowRecords::Profiles(profiles.validate()?))
}

fn set_nonempty(
    profiles: &mut Profiles,
    payload_type: ArrowPayloadType,
    batch: RecordBatch,
) -> Result<()> {
    if batch.num_rows() > 0 {
        profiles.set(payload_type, mark_parent_id_plain(batch))?;
    }
    Ok(())
}

fn mark_parent_id_plain(batch: RecordBatch) -> RecordBatch {
    if batch.column_by_name(consts::PARENT_ID).is_none() {
        return batch;
    }
    let schema = update_field_metadata(
        batch.schema_ref(),
        consts::PARENT_ID,
        consts::metadata::COLUMN_ENCODING,
        consts::metadata::encodings::PLAIN,
    );
    RecordBatch::try_new(Arc::new(schema), batch.columns().to_vec())
        .expect("Profiles parent metadata preserves record batch validity")
}

fn take_next_id(next: &mut u32) -> Result<u32> {
    let id = *next;
    *next = next.checked_add(1).ok_or(Error::U32OverflowError)?;
    Ok(id)
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidProfilesData {
        message: message.into(),
    }
}

#[derive(Clone)]
struct ResolvedAttribute {
    key: String,
    value: Option<AnyValue>,
    unit: Option<String>,
}

struct DictionaryResolver<'a> {
    dictionary: &'a ProfilesDictionary,
    attributes: Vec<Option<ResolvedAttribute>>,
}

impl<'a> DictionaryResolver<'a> {
    fn new(dictionary: &'a ProfilesDictionary) -> Result<Self> {
        validate_zero_entry(&dictionary.mapping_table, "mapping_table")?;
        validate_zero_entry(&dictionary.location_table, "location_table")?;
        validate_zero_entry(&dictionary.function_table, "function_table")?;
        validate_zero_link(&dictionary.link_table)?;
        validate_zero_string(&dictionary.string_table)?;
        validate_zero_attribute(&dictionary.attribute_table)?;
        validate_zero_entry(&dictionary.stack_table, "stack_table")?;
        let total_string_bytes = dictionary
            .string_table
            .iter()
            .try_fold(0_usize, |total, value| total.checked_add(value.len()))
            .ok_or_else(|| invalid("string_table byte length overflows usize"))?;
        if total_string_bytes > i32::MAX as usize {
            return Err(invalid(
                "string_table exceeds Arrow Utf8 value-offset capacity",
            ));
        }

        let mut resolver = Self {
            dictionary,
            attributes: vec![None; dictionary.attribute_table.len()],
        };
        for index in 1..dictionary.attribute_table.len() {
            let attribute =
                resolver.resolve_attribute(&dictionary.attribute_table[index], index)?;
            resolver.attributes[index] = Some(attribute);
        }
        Ok(resolver)
    }

    fn resolve_string(&self, index: i32, field: &str) -> Result<&'a str> {
        let index = nonnegative_index(index, field)?;
        self.dictionary
            .string_table
            .get(index)
            .map(String::as_str)
            .ok_or_else(|| {
                invalid(format!(
                    "{field} string index {index} is outside string_table length {}",
                    self.dictionary.string_table.len()
                ))
            })
    }

    fn optional_string(&self, index: i32, field: &str) -> Result<Option<&'a str>> {
        let value = self.resolve_string(index, field)?;
        Ok((!value.is_empty()).then_some(value))
    }

    fn entity_id(&self, index: i32, len: usize, field: &str) -> Result<Option<u32>> {
        let index = nonnegative_index(index, field)?;
        if index == 0 {
            return Ok(None);
        }
        if index >= len {
            return Err(invalid(format!(
                "{field} index {index} is outside table length {len}"
            )));
        }
        Ok(Some(
            u32::try_from(index).map_err(|_| Error::U32OverflowError)?,
        ))
    }

    fn mapping_id(&self, index: i32, field: &str) -> Result<Option<u32>> {
        self.entity_id(index, self.dictionary.mapping_table.len(), field)
    }

    fn location_id(&self, index: i32, field: &str) -> Result<Option<u32>> {
        self.entity_id(index, self.dictionary.location_table.len(), field)
    }

    fn function_id(&self, index: i32, field: &str) -> Result<Option<u32>> {
        self.entity_id(index, self.dictionary.function_table.len(), field)
    }

    fn link_id(&self, index: i32, field: &str) -> Result<Option<u32>> {
        self.entity_id(index, self.dictionary.link_table.len(), field)
    }

    fn stack_id(&self, index: i32, field: &str) -> Result<Option<u32>> {
        self.entity_id(index, self.dictionary.stack_table.len(), field)
    }

    fn attribute(&self, index: i32, field: &str) -> Result<Option<&ResolvedAttribute>> {
        let index = nonnegative_index(index, field)?;
        if index == 0 {
            return Ok(None);
        }
        self.attributes
            .get(index)
            .and_then(Option::as_ref)
            .map(Some)
            .ok_or_else(|| {
                invalid(format!(
                    "{field} index {index} is outside attribute_table length {}",
                    self.attributes.len()
                ))
            })
    }

    fn resolve_attribute(
        &self,
        attribute: &KeyValueAndUnit,
        index: usize,
    ) -> Result<ResolvedAttribute> {
        if attribute == &KeyValueAndUnit::default() {
            return Err(invalid(format!(
                "attribute_table[{index}] is a nonzero zero-value entry"
            )));
        }
        let key = self
            .resolve_string(attribute.key_strindex, "attribute.key_strindex")?
            .to_string();
        if key.is_empty() {
            return Err(invalid(format!(
                "attribute_table[{index}] resolves to an empty key"
            )));
        }
        let value = attribute
            .value
            .as_ref()
            .map(|value| self.resolve_any_value(value, 0))
            .transpose()?;
        let unit = self
            .optional_string(attribute.unit_strindex, "attribute.unit_strindex")?
            .map(str::to_string);
        Ok(ResolvedAttribute { key, value, unit })
    }

    fn resolve_key_value(&self, kv: &KeyValue, depth: usize) -> Result<KeyValue> {
        if !kv.key.is_empty() && kv.key_strindex != 0 {
            return Err(invalid(
                "KeyValue sets both key and key_strindex in Profiles data",
            ));
        }
        let key = if kv.key.is_empty() {
            self.resolve_string(kv.key_strindex, "KeyValue.key_strindex")?
                .to_string()
        } else {
            kv.key.clone()
        };
        if key.is_empty() {
            return Err(invalid("Profiles attribute key is empty"));
        }
        let value = kv
            .value
            .as_ref()
            .map(|value| self.resolve_any_value(value, depth))
            .transpose()?;
        Ok(KeyValue {
            key,
            value,
            key_strindex: 0,
        })
    }

    fn resolve_any_value(&self, value: &AnyValue, depth: usize) -> Result<AnyValue> {
        if depth >= MAX_ANY_VALUE_DEPTH {
            return Err(invalid(format!(
                "Profiles AnyValue nesting exceeds {MAX_ANY_VALUE_DEPTH}"
            )));
        }
        let resolved = match value.value.as_ref() {
            Some(any_value::Value::StringValueStrindex(index)) => {
                Some(any_value::Value::StringValue(
                    self.resolve_string(*index, "AnyValue.string_value_strindex")?
                        .to_string(),
                ))
            }
            Some(any_value::Value::ArrayValue(array)) => {
                let values = array
                    .values
                    .iter()
                    .map(|value| self.resolve_any_value(value, depth + 1))
                    .collect::<Result<Vec<_>>>()?;
                Some(any_value::Value::ArrayValue(
                    crate::proto::opentelemetry::common::v1::ArrayValue { values },
                ))
            }
            Some(any_value::Value::KvlistValue(list)) => {
                let mut keys = HashSet::with_capacity(list.values.len());
                let values = list
                    .values
                    .iter()
                    .map(|kv| {
                        let kv = self.resolve_key_value(kv, depth + 1)?;
                        if !keys.insert(kv.key.clone()) {
                            return Err(invalid(format!(
                                "duplicate key {:?} in nested Profiles KeyValueList",
                                kv.key
                            )));
                        }
                        Ok(kv)
                    })
                    .collect::<Result<Vec<_>>>()?;
                Some(any_value::Value::KvlistValue(
                    crate::proto::opentelemetry::common::v1::KeyValueList { values },
                ))
            }
            other => other.cloned(),
        };
        Ok(AnyValue { value: resolved })
    }
}

fn nonnegative_index(index: i32, field: &str) -> Result<usize> {
    usize::try_from(index).map_err(|_| invalid(format!("{field} contains negative index {index}")))
}

fn validate_zero_entry<T>(table: &[T], table_name: &str) -> Result<()>
where
    T: Default + PartialEq,
{
    match table.first() {
        Some(value) if value == &T::default() => Ok(()),
        Some(_) => Err(invalid(format!(
            "{table_name}[0] is not the required zero value"
        ))),
        None => Err(invalid(format!("{table_name} is missing index zero"))),
    }
}

fn validate_zero_link(table: &[Link]) -> Result<()> {
    let Some(link) = table.first() else {
        return Err(invalid("link_table is missing index zero"));
    };
    let valid_trace = link.trace_id.is_empty()
        || (link.trace_id.len() == 16 && link.trace_id.iter().all(|byte| *byte == 0));
    let valid_span = link.span_id.is_empty()
        || (link.span_id.len() == 8 && link.span_id.iter().all(|byte| *byte == 0));
    if valid_trace && valid_span {
        Ok(())
    } else {
        Err(invalid("link_table[0] is not the required zero value"))
    }
}

fn validate_zero_string(table: &[String]) -> Result<()> {
    match table.first() {
        Some(value) if value.is_empty() => Ok(()),
        Some(_) => Err(invalid("string_table[0] is not empty")),
        None => Err(invalid("string_table is missing index zero")),
    }
}

fn validate_zero_attribute(table: &[KeyValueAndUnit]) -> Result<()> {
    let Some(attribute) = table.first() else {
        return Err(invalid("attribute_table is missing index zero"));
    };
    let zero_value = attribute
        .value
        .as_ref()
        .is_none_or(|value| value == &AnyValue::default());
    if attribute.key_strindex == 0 && attribute.unit_strindex == 0 && zero_value {
        Ok(())
    } else {
        Err(invalid("attribute_table[0] is not the required zero value"))
    }
}

fn append_direct_attributes(
    builder: &mut AttributesRecordBatchBuilder<u16>,
    parent_id: u16,
    attributes: &[KeyValue],
    resolver: &DictionaryResolver<'_>,
    owner: &str,
) -> Result<()> {
    let mut keys = HashSet::with_capacity(attributes.len());
    for kv in attributes {
        let kv = resolver.resolve_key_value(kv, 0)?;
        if !keys.insert(kv.key.clone()) {
            return Err(invalid(format!(
                "duplicate attribute key {:?} on {owner}",
                kv.key
            )));
        }
        builder.append_parent_id(&parent_id);
        builder.append_key(kv.key.as_bytes());
        append_any_value(&mut builder.any_values_builder, kv.value.as_ref())?;
    }
    Ok(())
}

fn append_referenced_attributes(
    builder: &mut ProfileAttributesBuilder,
    parent_id: u32,
    indices: &[i32],
    resolver: &DictionaryResolver<'_>,
    owner: &str,
) -> Result<usize> {
    let mut keys = HashSet::with_capacity(indices.len());
    let mut ordinal = 0_u32;
    for index in indices {
        let Some(attribute) = resolver.attribute(*index, "attribute_indices")? else {
            continue;
        };
        if !keys.insert(attribute.key.clone()) {
            return Err(invalid(format!(
                "duplicate attribute key {:?} on {owner}",
                attribute.key
            )));
        }
        builder.append(parent_id, ordinal, attribute)?;
        ordinal = ordinal.checked_add(1).ok_or(Error::U32OverflowError)?;
    }
    Ok(ordinal as usize)
}

fn append_any_value(builder: &mut AnyValuesRecordsBuilder, value: Option<&AnyValue>) -> Result<()> {
    match value.and_then(|value| value.value.as_ref()) {
        Some(any_value::Value::StringValue(value)) => builder.append_str(value.as_bytes()),
        Some(any_value::Value::BoolValue(value)) => builder.append_bool(*value),
        Some(any_value::Value::IntValue(value)) => builder.append_int(*value),
        Some(any_value::Value::DoubleValue(value)) => builder.append_double(*value),
        Some(any_value::Value::BytesValue(value)) => builder.append_bytes(value),
        Some(any_value::Value::ArrayValue(array)) => {
            let mut serialized = Vec::new();
            serialize_any_values(array.values.iter().map(ObjAny::new), &mut serialized)?;
            builder.append_slice(&serialized);
        }
        Some(any_value::Value::KvlistValue(list)) => {
            let mut serialized = Vec::new();
            serialize_kv_list(
                list.values.iter().map(|kv| {
                    ObjKeyValue::new(kv.key.as_str(), kv.value.as_ref().map(ObjAny::new))
                }),
                &mut serialized,
            )?;
            builder.append_map(&serialized);
        }
        Some(any_value::Value::StringValueStrindex(index)) => {
            return Err(invalid(format!("unresolved AnyValue string index {index}")));
        }
        None => builder.append_empty(),
    }
    Ok(())
}

struct ProfileAttributesBuilder {
    parent_ids: Vec<u32>,
    ordinals: Vec<u32>,
    keys: BinaryArrayBuilder,
    values: AnyValuesRecordsBuilder,
    units: BinaryArrayBuilder,
}

impl Default for ProfileAttributesBuilder {
    fn default() -> Self {
        Self {
            parent_ids: Vec::new(),
            ordinals: Vec::new(),
            keys: BinaryArrayBuilder::new(ArrayOptions {
                optional: false,
                dictionary_options: Some(DictionaryOptions::dict8()),
                ..Default::default()
            }),
            values: AnyValuesRecordsBuilder::new(),
            units: BinaryArrayBuilder::new(ArrayOptions {
                optional: true,
                dictionary_options: Some(DictionaryOptions::dict8()),
                ..Default::default()
            }),
        }
    }
}

impl ProfileAttributesBuilder {
    fn append(
        &mut self,
        parent_id: u32,
        ordinal: u32,
        attribute: &ResolvedAttribute,
    ) -> Result<()> {
        self.parent_ids.push(parent_id);
        self.ordinals.push(ordinal);
        self.keys.append_slice(attribute.key.as_bytes());
        if let Some(unit) = &attribute.unit {
            self.units.append_slice(unit.as_bytes());
        } else {
            self.units.append_null();
        }
        append_any_value(&mut self.values, attribute.value.as_ref())
    }

    fn finish(mut self) -> Result<RecordBatch> {
        let mut fields = vec![
            Field::new(consts::PARENT_ID, DataType::UInt32, false),
            Field::new(consts::ORDINAL, DataType::UInt32, false),
        ];
        let mut columns: Vec<ArrayRef> = vec![
            Arc::new(UInt32Array::from(self.parent_ids)),
            Arc::new(UInt32Array::from(self.ordinals)),
        ];
        if let Some(keys) = self.keys.finish() {
            let keys = binary_to_utf8_array(&keys)?;
            fields.push(Field::new(
                consts::ATTRIBUTE_KEY,
                keys.data_type().clone(),
                false,
            ));
            columns.push(keys);
        }
        self.values.finish(&mut columns, &mut fields)?;
        if let Some(units) = self.units.finish() {
            let units = binary_to_utf8_array(&units)?;
            fields.push(Field::new(consts::UNIT, units.data_type().clone(), true));
            columns.push(units);
        }
        Ok(RecordBatch::try_new(
            Arc::new(Schema::new(fields)),
            columns,
        )?)
    }
}

struct ProfilesRootBuilder {
    ids: Vec<u32>,
    resource: ResourceBuilder,
    scope: ScopeBuilder,
    schema_urls: BinaryArrayBuilder,
    time_unix_nano: Vec<u64>,
    duration_nano: Vec<u64>,
    periods: Vec<Option<i64>>,
    profile_ids: Vec<Option<Vec<u8>>>,
    dropped_attributes: Vec<Option<u32>>,
    original_payload_formats: BinaryArrayBuilder,
    original_payloads: Vec<Option<Vec<u8>>>,
}

impl Default for ProfilesRootBuilder {
    fn default() -> Self {
        Self {
            ids: Vec::new(),
            resource: ResourceBuilder::new(),
            scope: ScopeBuilder::new(),
            schema_urls: BinaryArrayBuilder::new(ArrayOptions {
                optional: true,
                dictionary_options: Some(DictionaryOptions::dict8()),
                ..Default::default()
            }),
            time_unix_nano: Vec::new(),
            duration_nano: Vec::new(),
            periods: Vec::new(),
            profile_ids: Vec::new(),
            dropped_attributes: Vec::new(),
            original_payload_formats: BinaryArrayBuilder::new(ArrayOptions {
                optional: true,
                dictionary_options: Some(DictionaryOptions::dict8()),
                ..Default::default()
            }),
            original_payloads: Vec::new(),
        }
    }
}

impl ProfilesRootBuilder {
    #[allow(clippy::too_many_arguments)]
    fn append(
        &mut self,
        id: u32,
        resource_id: u16,
        resource_profiles: &ResourceProfiles,
        scope_id: u16,
        scope_profiles: &crate::proto::opentelemetry::profiles::v1development::ScopeProfiles,
        profile: &crate::proto::opentelemetry::profiles::v1development::Profile,
    ) -> Result<()> {
        let profile_id = if profile.profile_id.is_empty() {
            None
        } else if profile.profile_id.len() != 16 || profile.profile_id.iter().all(|byte| *byte == 0)
        {
            return Err(invalid(format!("profile {id} has an invalid profile_id")));
        } else {
            Some(profile.profile_id.clone())
        };

        let format_present = !profile.original_payload_format.is_empty();
        let payload_present = !profile.original_payload.is_empty();
        if format_present != payload_present {
            return Err(invalid(format!(
                "profile {id} must set original_payload_format and original_payload together"
            )));
        }
        _ = profile
            .time_unix_nano
            .checked_add(profile.duration_nano)
            .ok_or_else(|| invalid(format!("profile {id} time plus duration overflows u64")))?;

        self.ids.push(id);
        self.resource.append_id(Some(resource_id));
        self.resource.append_schema_url(
            (!resource_profiles.schema_url.is_empty())
                .then_some(resource_profiles.schema_url.as_bytes()),
        );
        self.resource.append_dropped_attributes_count(
            resource_profiles
                .resource
                .as_ref()
                .map_or(0, |resource| resource.dropped_attributes_count),
        );
        self.scope.append_id(Some(scope_id));
        self.scope.append_name(
            scope_profiles
                .scope
                .as_ref()
                .and_then(|scope| (!scope.name.is_empty()).then_some(scope.name.as_bytes())),
        );
        self.scope.append_version(
            scope_profiles
                .scope
                .as_ref()
                .and_then(|scope| (!scope.version.is_empty()).then_some(scope.version.as_bytes())),
        );
        self.scope.append_dropped_attributes_count(
            scope_profiles
                .scope
                .as_ref()
                .map_or(0, |scope| scope.dropped_attributes_count),
        );
        if scope_profiles.schema_url.is_empty() {
            self.schema_urls.append_null();
        } else {
            self.schema_urls
                .append_slice(scope_profiles.schema_url.as_bytes());
        }
        self.time_unix_nano.push(profile.time_unix_nano);
        self.duration_nano.push(profile.duration_nano);
        self.periods
            .push((profile.period != 0).then_some(profile.period));
        self.profile_ids.push(profile_id);
        self.dropped_attributes.push(
            (profile.dropped_attributes_count != 0).then_some(profile.dropped_attributes_count),
        );
        if format_present {
            self.original_payload_formats
                .append_slice(profile.original_payload_format.as_bytes());
        } else {
            self.original_payload_formats.append_null();
        }
        self.original_payloads
            .push(payload_present.then(|| profile.original_payload.clone()));
        Ok(())
    }

    fn finish(mut self) -> Result<RecordBatch> {
        let resources = self.resource.finish()?;
        let scopes = self.scope.finish()?;
        let profile_ids =
            FixedSizeBinaryArray::try_from_sparse_iter_with_size(self.profile_ids.into_iter(), 16)?;
        let original_payloads = LargeBinaryArray::from(
            self.original_payloads
                .iter()
                .map(|value| value.as_deref())
                .collect::<Vec<_>>(),
        );

        let mut fields = vec![
            Field::new(consts::ID, DataType::UInt32, false),
            Field::new(consts::RESOURCE, resources.data_type().clone(), true),
            Field::new(consts::SCOPE, scopes.data_type().clone(), true),
        ];
        let mut columns: Vec<ArrayRef> = vec![
            Arc::new(UInt32Array::from(self.ids)),
            Arc::new(resources),
            Arc::new(scopes),
        ];
        if let Some(schema_urls) = self.schema_urls.finish() {
            let schema_urls = binary_to_utf8_array(&schema_urls)?;
            fields.push(Field::new(
                consts::SCHEMA_URL,
                schema_urls.data_type().clone(),
                true,
            ));
            columns.push(schema_urls);
        }
        fields.extend([
            Field::new(consts::TIME_UNIX_NANO, DataType::UInt64, false),
            Field::new(consts::DURATION_NANO, DataType::UInt64, false),
            Field::new(consts::PERIOD, DataType::Int64, true),
            Field::new(consts::PROFILE_ID, DataType::FixedSizeBinary(16), true),
            Field::new(consts::DROPPED_ATTRIBUTES_COUNT, DataType::UInt32, true),
        ]);
        columns.extend([
            Arc::new(UInt64Array::from(self.time_unix_nano)) as ArrayRef,
            Arc::new(UInt64Array::from(self.duration_nano)) as ArrayRef,
            Arc::new(Int64Array::from(self.periods)) as ArrayRef,
            Arc::new(profile_ids) as ArrayRef,
            Arc::new(UInt32Array::from(self.dropped_attributes)) as ArrayRef,
        ]);
        if let Some(formats) = self.original_payload_formats.finish() {
            let formats = binary_to_utf8_array(&formats)?;
            fields.push(Field::new(
                consts::ORIGINAL_PAYLOAD_FORMAT,
                formats.data_type().clone(),
                true,
            ));
            columns.push(formats);
        }
        fields.push(Field::new(
            consts::ORIGINAL_PAYLOAD,
            DataType::LargeBinary,
            true,
        ));
        columns.push(Arc::new(original_payloads));
        Ok(RecordBatch::try_new(
            Arc::new(Schema::new(fields)),
            columns,
        )?)
    }
}

struct ValueTypesBuilder {
    parent_ids: Vec<u32>,
    roles: Vec<u8>,
    types: BinaryArrayBuilder,
    units: BinaryArrayBuilder,
}

impl Default for ValueTypesBuilder {
    fn default() -> Self {
        Self {
            parent_ids: Vec::new(),
            roles: Vec::new(),
            types: BinaryArrayBuilder::new(ArrayOptions {
                optional: false,
                dictionary_options: Some(DictionaryOptions::dict8()),
                ..Default::default()
            }),
            units: BinaryArrayBuilder::new(ArrayOptions {
                optional: false,
                dictionary_options: Some(DictionaryOptions::dict8()),
                ..Default::default()
            }),
        }
    }
}

fn append_value_type(
    builder: &mut ValueTypesBuilder,
    resolver: &DictionaryResolver<'_>,
    profile_id: u32,
    role: u8,
    value_type: &ValueType,
) -> Result<()> {
    let r#type = resolver.resolve_string(value_type.type_strindex, "ValueType.type_strindex")?;
    let unit = resolver.resolve_string(value_type.unit_strindex, "ValueType.unit_strindex")?;
    if r#type.is_empty() && unit.is_empty() {
        return Ok(());
    }
    builder.parent_ids.push(profile_id);
    builder.roles.push(role);
    builder.types.append_slice(r#type.as_bytes());
    builder.units.append_slice(unit.as_bytes());
    Ok(())
}

impl ValueTypesBuilder {
    fn finish(mut self) -> Result<RecordBatch> {
        let mut fields = vec![
            Field::new(consts::PARENT_ID, DataType::UInt32, false),
            Field::new(consts::ROLE, DataType::UInt8, false),
        ];
        let mut columns: Vec<ArrayRef> = vec![
            Arc::new(UInt32Array::from(self.parent_ids)),
            Arc::new(UInt8Array::from(self.roles)),
        ];
        if let Some(types) = self.types.finish() {
            let types = binary_to_utf8_array(&types)?;
            fields.push(Field::new(
                consts::ATTRIBUTE_TYPE,
                types.data_type().clone(),
                false,
            ));
            columns.push(types);
        }
        if let Some(units) = self.units.finish() {
            let units = binary_to_utf8_array(&units)?;
            fields.push(Field::new(consts::UNIT, units.data_type().clone(), false));
            columns.push(units);
        }
        Ok(RecordBatch::try_new(
            Arc::new(Schema::new(fields)),
            columns,
        )?)
    }
}

#[derive(Default)]
struct SamplesBuilder {
    ids: Vec<u32>,
    parent_ids: Vec<u32>,
    stack_ids: Vec<Option<u32>>,
    link_ids: Vec<Option<u32>>,
    values: Vec<Vec<i64>>,
    timestamps: Vec<Vec<u64>>,
}

impl SamplesBuilder {
    fn append(
        &mut self,
        id: u32,
        parent_id: u32,
        sample: &crate::proto::opentelemetry::profiles::v1development::Sample,
        resolver: &DictionaryResolver<'_>,
    ) -> Result<()> {
        if sample.values.is_empty() && sample.timestamps_unix_nano.is_empty() {
            return Err(invalid(format!("sample {id} has no values or timestamps")));
        }
        if !sample.values.is_empty()
            && !sample.timestamps_unix_nano.is_empty()
            && sample.values.len() != sample.timestamps_unix_nano.len()
        {
            return Err(invalid(format!(
                "sample {id} values and timestamps have different lengths"
            )));
        }
        self.ids.push(id);
        self.parent_ids.push(parent_id);
        self.stack_ids
            .push(resolver.stack_id(sample.stack_index, "Sample.stack_index")?);
        self.link_ids
            .push(resolver.link_id(sample.link_index, "Sample.link_index")?);
        self.values.push(sample.values.clone());
        self.timestamps.push(sample.timestamps_unix_nano.clone());
        Ok(())
    }

    fn finish(self) -> Result<RecordBatch> {
        Ok(RecordBatch::try_from_iter([
            (
                consts::ID,
                Arc::new(UInt32Array::from(self.ids)) as ArrayRef,
            ),
            (
                consts::PARENT_ID,
                Arc::new(UInt32Array::from(self.parent_ids)) as ArrayRef,
            ),
            (
                consts::STACK_ID,
                Arc::new(UInt32Array::from(self.stack_ids)) as ArrayRef,
            ),
            (
                consts::LINK_ID,
                Arc::new(UInt32Array::from(self.link_ids)) as ArrayRef,
            ),
            (
                consts::VALUES,
                Arc::new(large_list_i64(&self.values)?) as ArrayRef,
            ),
            (
                consts::TIMESTAMPS_UNIX_NANO,
                Arc::new(large_list_u64(&self.timestamps)?) as ArrayRef,
            ),
        ])?)
    }
}

fn large_list_i64(rows: &[Vec<i64>]) -> Result<LargeListArray> {
    let (offsets, values) = large_list_parts(rows)?;
    Ok(LargeListArray::new(
        Arc::new(Field::new("item", DataType::Int64, true)),
        offsets,
        Arc::new(Int64Array::new(ScalarBuffer::from(values), None)),
        None,
    ))
}

fn large_list_u64(rows: &[Vec<u64>]) -> Result<LargeListArray> {
    let (offsets, values) = large_list_parts(rows)?;
    Ok(LargeListArray::new(
        Arc::new(Field::new("item", DataType::UInt64, true)),
        offsets,
        Arc::new(UInt64Array::new(ScalarBuffer::from(values), None)),
        None,
    ))
}

fn large_list_parts<T: Copy>(rows: &[Vec<T>]) -> Result<(OffsetBuffer<i64>, Vec<T>)> {
    let mut offsets = Vec::with_capacity(rows.len() + 1);
    let mut values = Vec::new();
    let mut offset = 0_i64;
    offsets.push(offset);
    for row in rows {
        offset = offset
            .checked_add(i64::try_from(row.len()).map_err(|_| Error::U32OverflowError)?)
            .ok_or(Error::U32OverflowError)?;
        offsets.push(offset);
        values.extend_from_slice(row);
    }
    Ok((OffsetBuffer::new(ScalarBuffer::from(offsets)), values))
}

fn optional_dictionary_string_builder() -> BinaryArrayBuilder {
    BinaryArrayBuilder::new(ArrayOptions {
        optional: true,
        dictionary_options: Some(DictionaryOptions::dict8()),
        ..Default::default()
    })
}

fn append_optional_string(builder: &mut BinaryArrayBuilder, value: Option<&str>) {
    if let Some(value) = value {
        builder.append_slice(value.as_bytes());
    } else {
        builder.append_null();
    }
}

fn append_optional_string_column(
    fields: &mut Vec<Field>,
    columns: &mut Vec<ArrayRef>,
    name: &'static str,
    builder: &mut BinaryArrayBuilder,
) -> Result<()> {
    if let Some(array) = builder.finish() {
        let array = binary_to_utf8_array(&array)?;
        fields.push(Field::new(name, array.data_type().clone(), true));
        columns.push(array);
    }
    Ok(())
}

fn build_mappings(
    resolver: &DictionaryResolver<'_>,
    attributes: &mut ProfileAttributesBuilder,
) -> Result<RecordBatch> {
    let mut ids = Vec::new();
    let mut memory_start = Vec::new();
    let mut memory_limit = Vec::new();
    let mut file_offset = Vec::new();
    let mut filenames = BinaryArrayBuilder::new(ArrayOptions {
        optional: true,
        dictionary_options: Some(DictionaryOptions::dict8()),
        ..Default::default()
    });
    for (index, mapping) in resolver.dictionary.mapping_table.iter().enumerate().skip(1) {
        if mapping == &Mapping::default() {
            return Err(invalid(format!(
                "mapping_table[{index}] is a nonzero zero-value entry"
            )));
        }
        if mapping.memory_start != 0
            && mapping.memory_limit != 0
            && mapping.memory_limit < mapping.memory_start
        {
            return Err(invalid(format!(
                "mapping_table[{index}] has an invalid address range"
            )));
        }
        let filename =
            resolver.optional_string(mapping.filename_strindex, "Mapping.filename_strindex")?;
        let id = u32::try_from(index).map_err(|_| Error::U32OverflowError)?;
        let attribute_count = append_referenced_attributes(
            attributes,
            id,
            &mapping.attribute_indices,
            resolver,
            "mapping",
        )?;
        if mapping.memory_start == 0
            && mapping.memory_limit == 0
            && mapping.file_offset == 0
            && filename.is_none()
            && attribute_count == 0
        {
            return Err(invalid(format!(
                "mapping_table[{index}] resolves to a zero-value entity"
            )));
        }
        ids.push(id);
        memory_start.push(mapping.memory_start);
        memory_limit.push(mapping.memory_limit);
        file_offset.push(mapping.file_offset);
        if let Some(filename) = filename {
            filenames.append_slice(filename.as_bytes());
        } else {
            filenames.append_null();
        }
    }
    let mut fields = vec![
        Field::new(consts::ID, DataType::UInt32, false),
        Field::new(consts::MEMORY_START, DataType::UInt64, false),
        Field::new(consts::MEMORY_LIMIT, DataType::UInt64, false),
        Field::new(consts::FILE_OFFSET, DataType::UInt64, false),
    ];
    let mut columns: Vec<ArrayRef> = vec![
        Arc::new(UInt32Array::from(ids)),
        Arc::new(UInt64Array::from(memory_start)),
        Arc::new(UInt64Array::from(memory_limit)),
        Arc::new(UInt64Array::from(file_offset)),
    ];
    if let Some(filenames) = filenames.finish() {
        let filenames = binary_to_utf8_array(&filenames)?;
        fields.push(Field::new(
            consts::FILENAME,
            filenames.data_type().clone(),
            true,
        ));
        columns.push(filenames);
    }
    Ok(RecordBatch::try_new(
        Arc::new(Schema::new(fields)),
        columns,
    )?)
}

fn build_functions(resolver: &DictionaryResolver<'_>) -> Result<RecordBatch> {
    let mut ids = Vec::new();
    let mut names = optional_dictionary_string_builder();
    let mut system_names = optional_dictionary_string_builder();
    let mut filenames = optional_dictionary_string_builder();
    let mut start_lines = Vec::new();
    for (index, function) in resolver
        .dictionary
        .function_table
        .iter()
        .enumerate()
        .skip(1)
    {
        if function == &Function::default() {
            return Err(invalid(format!(
                "function_table[{index}] is a nonzero zero-value entry"
            )));
        }
        let name = resolver.optional_string(function.name_strindex, "Function.name_strindex")?;
        let system_name = resolver.optional_string(
            function.system_name_strindex,
            "Function.system_name_strindex",
        )?;
        let filename =
            resolver.optional_string(function.filename_strindex, "Function.filename_strindex")?;
        if name.is_none() && system_name.is_none() && filename.is_none() {
            return Err(invalid(format!(
                "function_table[{index}] has no name, system_name, or filename"
            )));
        }
        if function.start_line < 0 {
            return Err(invalid(format!(
                "function_table[{index}] has a negative start_line"
            )));
        }
        ids.push(u32::try_from(index).map_err(|_| Error::U32OverflowError)?);
        append_optional_string(&mut names, name);
        append_optional_string(&mut system_names, system_name);
        append_optional_string(&mut filenames, filename);
        start_lines.push((function.start_line != 0).then_some(function.start_line));
    }
    let mut fields = vec![Field::new(consts::ID, DataType::UInt32, false)];
    let mut columns: Vec<ArrayRef> = vec![Arc::new(UInt32Array::from(ids))];
    append_optional_string_column(&mut fields, &mut columns, consts::NAME, &mut names)?;
    append_optional_string_column(
        &mut fields,
        &mut columns,
        consts::SYSTEM_NAME,
        &mut system_names,
    )?;
    append_optional_string_column(&mut fields, &mut columns, consts::FILENAME, &mut filenames)?;
    fields.push(Field::new(consts::START_LINE, DataType::Int64, true));
    columns.push(Arc::new(Int64Array::from(start_lines)));
    Ok(RecordBatch::try_new(
        Arc::new(Schema::new(fields)),
        columns,
    )?)
}

fn build_links(resolver: &DictionaryResolver<'_>) -> Result<RecordBatch> {
    let mut ids = Vec::new();
    let mut trace_ids = Vec::new();
    let mut span_ids = Vec::new();
    for (index, link) in resolver.dictionary.link_table.iter().enumerate().skip(1) {
        validate_required_id(&link.trace_id, 16, "trace_id", index)?;
        validate_required_id(&link.span_id, 8, "span_id", index)?;
        ids.push(u32::try_from(index).map_err(|_| Error::U32OverflowError)?);
        trace_ids.push(link.trace_id.clone());
        span_ids.push(link.span_id.clone());
    }
    Ok(RecordBatch::try_from_iter([
        (consts::ID, Arc::new(UInt32Array::from(ids)) as ArrayRef),
        (
            consts::TRACE_ID,
            Arc::new(FixedSizeBinaryArray::try_from_sparse_iter_with_size(
                trace_ids.into_iter().map(Some),
                16,
            )?) as ArrayRef,
        ),
        (
            consts::SPAN_ID,
            Arc::new(FixedSizeBinaryArray::try_from_sparse_iter_with_size(
                span_ids.into_iter().map(Some),
                8,
            )?) as ArrayRef,
        ),
    ])?)
}

fn validate_required_id(value: &[u8], len: usize, field: &str, index: usize) -> Result<()> {
    if value.len() != len || value.iter().all(|byte| *byte == 0) {
        return Err(invalid(format!(
            "link_table[{index}] has an invalid {field}"
        )));
    }
    Ok(())
}

fn build_locations(
    resolver: &DictionaryResolver<'_>,
    attributes: &mut ProfileAttributesBuilder,
) -> Result<(RecordBatch, RecordBatch)> {
    let mut ids = Vec::new();
    let mut mapping_ids = Vec::new();
    let mut addresses = Vec::new();
    let mut line_parent_ids = Vec::new();
    let mut line_ordinals = Vec::new();
    let mut function_ids = Vec::new();
    let mut lines = Vec::new();
    let mut columns = Vec::new();
    for (index, location) in resolver
        .dictionary
        .location_table
        .iter()
        .enumerate()
        .skip(1)
    {
        if location == &Location::default() {
            return Err(invalid(format!(
                "location_table[{index}] is a nonzero zero-value entry"
            )));
        }
        let id = u32::try_from(index).map_err(|_| Error::U32OverflowError)?;
        let mapping_id = resolver.mapping_id(location.mapping_index, "Location.mapping_index")?;
        let attribute_count = append_referenced_attributes(
            attributes,
            id,
            &location.attribute_indices,
            resolver,
            "location",
        )?;
        if mapping_id.is_none()
            && location.address == 0
            && location.lines.is_empty()
            && attribute_count == 0
        {
            return Err(invalid(format!(
                "location_table[{index}] resolves to a zero-value entity"
            )));
        }
        ids.push(id);
        mapping_ids.push(mapping_id);
        addresses.push(location.address);
        for (ordinal, line) in location.lines.iter().enumerate() {
            if line.line < 0 || line.column < 0 {
                return Err(invalid(format!(
                    "location_table[{index}] has a negative line or column"
                )));
            }
            line_parent_ids.push(id);
            line_ordinals.push(u32::try_from(ordinal).map_err(|_| Error::U32OverflowError)?);
            function_ids.push(resolver.function_id(line.function_index, "Line.function_index")?);
            lines.push(line.line);
            columns.push(line.column);
        }
    }
    let locations = RecordBatch::try_from_iter([
        (consts::ID, Arc::new(UInt32Array::from(ids)) as ArrayRef),
        (
            consts::MAPPING_ID,
            Arc::new(UInt32Array::from(mapping_ids)) as ArrayRef,
        ),
        (
            consts::ADDRESS,
            Arc::new(UInt64Array::from(addresses)) as ArrayRef,
        ),
    ])?;
    let line_batch = RecordBatch::try_from_iter([
        (
            consts::PARENT_ID,
            Arc::new(UInt32Array::from(line_parent_ids)) as ArrayRef,
        ),
        (
            consts::ORDINAL,
            Arc::new(UInt32Array::from(line_ordinals)) as ArrayRef,
        ),
        (
            consts::FUNCTION_ID,
            Arc::new(UInt32Array::from(function_ids)) as ArrayRef,
        ),
        (consts::LINE, Arc::new(Int64Array::from(lines)) as ArrayRef),
        (
            consts::COLUMN,
            Arc::new(Int64Array::from(columns)) as ArrayRef,
        ),
    ])?;
    Ok((locations, line_batch))
}

fn build_stacks(resolver: &DictionaryResolver<'_>) -> Result<(RecordBatch, RecordBatch)> {
    let mut ids = Vec::new();
    let mut parent_ids = Vec::new();
    let mut ordinals = Vec::new();
    let mut location_ids = Vec::new();
    for (index, stack) in resolver.dictionary.stack_table.iter().enumerate().skip(1) {
        if stack == &Stack::default() {
            return Err(invalid(format!(
                "stack_table[{index}] is a nonzero zero-value entry"
            )));
        }
        let id = u32::try_from(index).map_err(|_| Error::U32OverflowError)?;
        ids.push(id);
        for (ordinal, location_index) in stack.location_indices.iter().enumerate() {
            parent_ids.push(id);
            ordinals.push(u32::try_from(ordinal).map_err(|_| Error::U32OverflowError)?);
            location_ids.push(resolver.location_id(*location_index, "Stack.location_indices")?);
        }
    }
    Ok((
        RecordBatch::try_from_iter([(consts::ID, Arc::new(UInt32Array::from(ids)) as ArrayRef)])?,
        RecordBatch::try_from_iter([
            (
                consts::PARENT_ID,
                Arc::new(UInt32Array::from(parent_ids)) as ArrayRef,
            ),
            (
                consts::ORDINAL,
                Arc::new(UInt32Array::from(ordinals)) as ArrayRef,
            ),
            (
                consts::LOCATION_ID,
                Arc::new(UInt32Array::from(location_ids)) as ArrayRef,
            ),
        ])?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TryIntoWithOptions;
    use crate::arrays::MaybeDictArrayAccessor;
    use crate::proto::OtlpProtoMessage;
    use crate::proto::opentelemetry::common::v1::EntityRef;
    use crate::proto::opentelemetry::common::v1::{ArrayValue, InstrumentationScope, KeyValueList};
    use crate::proto::opentelemetry::profiles::v1development::{
        Line, Profile, Sample, ScopeProfiles,
    };
    use crate::proto::opentelemetry::resource::v1::Resource;
    use crate::testing::equiv::assert_equivalent;
    use crate::{OtapPayload, OtlpProtoBytes};
    use arrow::array::{StringArray, StructArray};
    use prost::Message;

    fn zero_dictionary() -> ProfilesDictionary {
        ProfilesDictionary {
            mapping_table: vec![Mapping::default()],
            location_table: vec![Location::default()],
            function_table: vec![Function::default()],
            link_table: vec![Link::default()],
            string_table: vec![String::new()],
            attribute_table: vec![KeyValueAndUnit::default()],
            stack_table: vec![Stack::default()],
        }
    }

    fn minimal_data() -> ProfilesData {
        ProfilesData {
            resource_profiles: vec![ResourceProfiles {
                resource: None,
                scope_profiles: vec![ScopeProfiles {
                    scope: None,
                    profiles: vec![Profile {
                        samples: vec![Sample {
                            values: vec![7],
                            ..Default::default()
                        }],
                        time_unix_nano: 10,
                        duration_nano: 5,
                        ..Default::default()
                    }],
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
            dictionary: Some(zero_dictionary()),
        }
    }

    /// Scenario: A minimal OTLP Profiles message has one profile and one value-only sample.
    /// Guarantees: Encoding produces a graph-valid Profiles root and Samples payload.
    #[test]
    fn encodes_minimal_profiles_graph() {
        let records = encode_profiles_otap_batch(&minimal_data()).unwrap();

        assert_eq!(records.num_items(), 1);
        assert_eq!(
            records
                .get(ArrowPayloadType::Profiles)
                .expect("Profiles root")
                .num_rows(),
            1
        );
        assert_eq!(
            records
                .get(ArrowPayloadType::Samples)
                .expect("Samples payload")
                .num_rows(),
            1
        );
    }

    /// Scenario: eBPF-style sample attributes contain integer, bytes, and complex values.
    /// Guarantees: Shared dictionary encodings validate and survive canonical OTLP reconstruction.
    #[test]
    fn encodes_dictionary_backed_profile_attribute_values() {
        let mut data = minimal_data();
        let dictionary = data.dictionary.as_mut().unwrap();
        dictionary.string_table.extend(
            ["sample.int", "sample.bytes", "sample.complex"]
                .into_iter()
                .map(str::to_string),
        );
        dictionary.attribute_table.extend([
            KeyValueAndUnit {
                key_strindex: 1,
                value: Some(AnyValue {
                    value: Some(any_value::Value::IntValue(42)),
                }),
                unit_strindex: 0,
            },
            KeyValueAndUnit {
                key_strindex: 2,
                value: Some(AnyValue {
                    value: Some(any_value::Value::BytesValue(vec![1, 2, 3])),
                }),
                unit_strindex: 0,
            },
            KeyValueAndUnit {
                key_strindex: 3,
                value: Some(AnyValue {
                    value: Some(any_value::Value::ArrayValue(ArrayValue {
                        values: vec![AnyValue {
                            value: Some(any_value::Value::IntValue(7)),
                        }],
                    })),
                }),
                unit_strindex: 0,
            },
        ]);
        data.resource_profiles[0].scope_profiles[0].profiles[0].samples[0].attribute_indices =
            vec![1, 2, 3];

        let records = encode_profiles_otap_batch(&data).unwrap();
        let attrs = records
            .get(ArrowPayloadType::ProfileSampleAttrs)
            .expect("sample attributes");
        assert_eq!(
            attrs
                .column_by_name(consts::ATTRIBUTE_INT)
                .unwrap()
                .data_type(),
            &DataType::Dictionary(Box::new(DataType::UInt16), Box::new(DataType::Int64))
        );
        assert_eq!(
            attrs
                .column_by_name(consts::ATTRIBUTE_BYTES)
                .unwrap()
                .data_type(),
            &DataType::Dictionary(Box::new(DataType::UInt16), Box::new(DataType::Binary))
        );
        assert_eq!(
            attrs
                .column_by_name(consts::ATTRIBUTE_SER)
                .unwrap()
                .data_type(),
            &DataType::Dictionary(Box::new(DataType::UInt16), Box::new(DataType::Binary))
        );

        let bytes: OtlpProtoBytes = OtapPayload::from_otap(records)
            .try_into_with_default()
            .unwrap();
        let decoded: OtlpProtoMessage = bytes.try_into().unwrap();
        assert_equivalent(&[OtlpProtoMessage::Profiles(data)], &[decoded]);
    }

    /// Scenario: Profiles data uses every dictionary table and nested string references.
    /// Guarantees: Encoding materializes strings, IDs, ordered edges, attributes, and lists.
    #[test]
    fn encodes_full_profiles_graph_and_materializes_strings() {
        let mut dictionary = zero_dictionary();
        dictionary.string_table.extend(
            [
                "cpu",
                "nanoseconds",
                "binary",
                "function",
                "system",
                "source.rs",
                "profile.attr",
                "value",
                "resource.attr",
                "scope.attr",
                "nested.attr",
                "nested.value",
                "bytes",
            ]
            .into_iter()
            .map(str::to_string),
        );
        dictionary.attribute_table.push(KeyValueAndUnit {
            key_strindex: 7,
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValueStrindex(8)),
            }),
            unit_strindex: 13,
        });
        dictionary.mapping_table.push(Mapping {
            memory_start: 100,
            memory_limit: 200,
            file_offset: 10,
            filename_strindex: 3,
            attribute_indices: vec![1],
        });
        dictionary.function_table.push(Function {
            name_strindex: 4,
            system_name_strindex: 5,
            filename_strindex: 6,
            start_line: 11,
        });
        dictionary.location_table.push(Location {
            mapping_index: 1,
            address: 123,
            lines: vec![Line {
                function_index: 1,
                line: 12,
                column: 3,
            }],
            attribute_indices: vec![1],
        });
        dictionary.link_table.push(Link {
            trace_id: vec![1; 16],
            span_id: vec![2; 8],
        });
        dictionary.stack_table.push(Stack {
            location_indices: vec![1],
        });

        let nested_value = AnyValue {
            value: Some(any_value::Value::KvlistValue(KeyValueList {
                values: vec![KeyValue {
                    key: String::new(),
                    value: Some(AnyValue {
                        value: Some(any_value::Value::ArrayValue(ArrayValue {
                            values: vec![AnyValue {
                                value: Some(any_value::Value::StringValueStrindex(12)),
                            }],
                        })),
                    }),
                    key_strindex: 11,
                }],
            })),
        };
        let data = ProfilesData {
            resource_profiles: vec![ResourceProfiles {
                resource: Some(Resource {
                    attributes: vec![KeyValue {
                        key: String::new(),
                        value: Some(AnyValue {
                            value: Some(any_value::Value::StringValueStrindex(8)),
                        }),
                        key_strindex: 9,
                    }],
                    dropped_attributes_count: 1,
                    ..Default::default()
                }),
                scope_profiles: vec![ScopeProfiles {
                    scope: Some(InstrumentationScope {
                        name: "scope".to_string(),
                        version: "1.0".to_string(),
                        attributes: vec![KeyValue {
                            key: String::new(),
                            value: Some(nested_value),
                            key_strindex: 10,
                        }],
                        dropped_attributes_count: 2,
                    }),
                    profiles: vec![Profile {
                        sample_type: Some(ValueType {
                            type_strindex: 1,
                            unit_strindex: 2,
                        }),
                        samples: vec![Sample {
                            stack_index: 1,
                            attribute_indices: vec![1],
                            link_index: 1,
                            values: vec![5],
                            timestamps_unix_nano: vec![99],
                        }],
                        time_unix_nano: 1000,
                        duration_nano: 50,
                        period: 10,
                        profile_id: vec![3; 16],
                        dropped_attributes_count: 3,
                        original_payload_format: "pprof".to_string(),
                        original_payload: vec![4, 5],
                        attribute_indices: vec![1],
                        ..Default::default()
                    }],
                    schema_url: "scope-schema".to_string(),
                }],
                schema_url: "resource-schema".to_string(),
            }],
            dictionary: Some(dictionary),
        };

        let records = encode_profiles_otap_batch(&data).unwrap();
        for payload_type in [
            ArrowPayloadType::Profiles,
            ArrowPayloadType::ProfileValueTypes,
            ArrowPayloadType::Samples,
            ArrowPayloadType::Stacks,
            ArrowPayloadType::StackLocations,
            ArrowPayloadType::ProfileLocations,
            ArrowPayloadType::ProfileLocationLines,
            ArrowPayloadType::ProfileFunctions,
            ArrowPayloadType::ProfileMappings,
            ArrowPayloadType::ProfileLinks,
            ArrowPayloadType::ProfileAttrs,
            ArrowPayloadType::ProfileSampleAttrs,
            ArrowPayloadType::ProfileMappingAttrs,
            ArrowPayloadType::ProfileLocationAttrs,
            ArrowPayloadType::ResourceAttrs,
            ArrowPayloadType::ScopeAttrs,
        ] {
            assert!(
                records.get(payload_type).is_some(),
                "missing {payload_type:?}"
            );
        }

        let samples = records.get(ArrowPayloadType::Samples).unwrap();
        let stack_ids = samples
            .column_by_name(consts::STACK_ID)
            .unwrap()
            .as_any()
            .downcast_ref::<UInt32Array>()
            .unwrap();
        assert_eq!(stack_ids.value(0), 1);

        let profile_attrs = records.get(ArrowPayloadType::ProfileAttrs).unwrap();
        let keys = MaybeDictArrayAccessor::<StringArray>::try_new(
            profile_attrs.column_by_name(consts::ATTRIBUTE_KEY).unwrap(),
        )
        .unwrap();
        assert_eq!(keys.str_at(0), Some("profile.attr"));
        let values = MaybeDictArrayAccessor::<StringArray>::try_new(
            profile_attrs.column_by_name(consts::ATTRIBUTE_STR).unwrap(),
        )
        .unwrap();
        assert_eq!(values.str_at(0), Some("value"));
        let units = MaybeDictArrayAccessor::<StringArray>::try_new(
            profile_attrs.column_by_name(consts::UNIT).unwrap(),
        )
        .unwrap();
        assert_eq!(units.str_at(0), Some("bytes"));
    }

    /// Scenario: A sample references a stack index outside the request dictionary.
    /// Guarantees: Encoding rejects the complete Profiles request without producing a partial BAR.
    #[test]
    fn rejects_out_of_range_dictionary_reference() {
        let mut data = minimal_data();
        data.resource_profiles[0].scope_profiles[0].profiles[0].samples[0].stack_index = 1;

        assert!(matches!(
            encode_profiles_otap_batch(&data),
            Err(Error::InvalidProfilesData { .. })
        ));
    }

    /// Scenario: Serialized ExportProfilesServiceRequest bytes contain a valid minimal profile.
    /// Guarantees: The standard payload conversion entry point decodes and encodes Profiles.
    #[test]
    fn payload_conversion_encodes_profiles_request() {
        let data = minimal_data();
        let request = ExportProfilesServiceRequest {
            resource_profiles: data.resource_profiles,
            dictionary: data.dictionary,
        };
        let bytes = request.encode_to_vec();
        let records: OtapArrowRecords = OtlpProtoBytes::ExportProfilesRequest(bytes.into())
            .try_into_with_default()
            .unwrap();

        assert!(matches!(records, OtapArrowRecords::Profiles(_)));
        assert_eq!(records.num_items(), 1);
    }

    /// Scenario: Many profiles share one large resource schema URL.
    /// Guarantees: Root metadata stays dictionary encoded instead of copying the string per row.
    #[test]
    fn repeated_root_metadata_uses_dictionary_encoding() {
        let mut data = minimal_data();
        let profile = data.resource_profiles[0].scope_profiles[0].profiles[0].clone();
        data.resource_profiles[0].scope_profiles[0].profiles =
            vec![profile.clone(), profile.clone(), profile];
        data.resource_profiles[0].schema_url = "x".repeat(64 * 1024);

        let records = encode_profiles_otap_batch(&data).unwrap();
        let root = records.get(ArrowPayloadType::Profiles).unwrap();
        let resources = root
            .column_by_name(consts::RESOURCE)
            .unwrap()
            .as_any()
            .downcast_ref::<StructArray>()
            .unwrap();
        assert!(matches!(
            resources
                .column_by_name(consts::SCHEMA_URL)
                .unwrap()
                .data_type(),
            DataType::Dictionary(_, _)
        ));
    }

    /// Scenario: A Profiles resource contains entity references not represented by the OTAP schema.
    /// Guarantees: Encoding rejects the request instead of silently discarding entity semantics.
    #[test]
    fn rejects_unrepresentable_resource_entity_refs() {
        let mut data = minimal_data();
        data.resource_profiles[0].resource = Some(Resource {
            entity_refs: vec![EntityRef::default()],
            ..Default::default()
        });

        assert!(matches!(
            encode_profiles_otap_batch(&data),
            Err(Error::InvalidProfilesData { .. })
        ));
    }

    /// Scenario: A nonzero mapping resolves to all-zero OTAP fields through an empty string entry.
    /// Guarantees: Encoding rejects the canonical zero entity rather than assigning it an ID.
    #[test]
    fn rejects_mapping_that_resolves_to_zero_value() {
        let mut data = minimal_data();
        let dictionary = data.dictionary.as_mut().unwrap();
        dictionary.string_table.push(String::new());
        dictionary.mapping_table.push(Mapping {
            filename_strindex: 1,
            ..Default::default()
        });

        assert!(matches!(
            encode_profiles_otap_batch(&data),
            Err(Error::InvalidProfilesData { .. })
        ));
    }

    /// Scenario: Profile collection time plus duration exceeds the UInt64 time domain.
    /// Guarantees: Encoding rejects interval arithmetic overflow.
    #[test]
    fn rejects_profile_interval_overflow() {
        let mut data = minimal_data();
        let profile = &mut data.resource_profiles[0].scope_profiles[0].profiles[0];
        profile.time_unix_nano = u64::MAX;
        profile.duration_nano = 1;

        assert!(matches!(
            encode_profiles_otap_batch(&data),
            Err(Error::InvalidProfilesData { .. })
        ));
    }

    /// Scenario: A location contains a negative source line.
    /// Guarantees: Encoding enforces zero-or-one-based source positions.
    #[test]
    fn rejects_negative_source_position() {
        let mut data = minimal_data();
        data.dictionary
            .as_mut()
            .unwrap()
            .location_table
            .push(Location {
                address: 1,
                lines: vec![Line {
                    line: -1,
                    ..Default::default()
                }],
                ..Default::default()
            });

        assert!(matches!(
            encode_profiles_otap_batch(&data),
            Err(Error::InvalidProfilesData { .. })
        ));
    }
}
