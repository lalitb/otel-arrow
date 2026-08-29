// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! OTAP Profiles to OTLP Profiles encoding.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::mem::size_of;

use arrow::array::{
    Array, ArrayRef, Int64Array, LargeBinaryArray, LargeListArray, RecordBatch, StructArray,
    UInt8Array, UInt32Array, UInt64Array,
};
use arrow::datatypes::DataType as ArrowDataType;
use prost::Message;

use crate::arrays::{
    ByteArrayAccessor, MaybeDictArrayAccessor, NullableArrayAccessor, StringArrayAccessor,
    get_i64_array_opt, get_required_array, get_u32_array, get_u32_array_opt, get_u64_array,
};
use crate::error::{Error, Result};
use crate::otap::{OtapArrowRecords, OtapBatchStore, Profiles};
use crate::otlp::ProtoBytesEncoder;
use crate::otlp::attributes::{
    Attribute16Arrays, Attribute32Arrays, AttributeValueType, encode_any_value,
};
use crate::otlp::common::{AnyValueArrays, BoundedBuf, ProtoBuffer, ResourceArrays, ScopeArrays};
use crate::proto::opentelemetry::arrow::v1::ArrowPayloadType;
use crate::proto::opentelemetry::collector::profiles::v1development::ExportProfilesServiceRequest;
use crate::proto::opentelemetry::common::v1::{AnyValue, InstrumentationScope, KeyValue};
use crate::proto::opentelemetry::profiles::v1development::{
    Function, KeyValueAndUnit, Line, Link, Location, Mapping, Profile, ProfilesDictionary,
    ResourceProfiles, Sample, ScopeProfiles, Stack, ValueType,
};
use crate::proto::opentelemetry::resource::v1::Resource;
use crate::schema::consts;

/// Encoder for OTAP Profiles record batches.
#[derive(Default)]
pub struct ProfilesProtoBytesEncoder;

impl ProfilesProtoBytesEncoder {
    /// Create a Profiles protobuf encoder.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl ProtoBytesEncoder for ProfilesProtoBytesEncoder {
    fn encode(
        &mut self,
        otap_batch: &mut OtapArrowRecords,
        result_buf: &mut ProtoBuffer,
    ) -> Result<()> {
        let logical_arrow_bytes = otap_batch.logical_arrow_bytes()?;
        let OtapArrowRecords::Profiles(profiles) = otap_batch else {
            return Err(unexpected("Profiles encoder received a non-Profiles batch"));
        };
        if profiles.get(ArrowPayloadType::Profiles).is_none() {
            if Profiles::allowed_payload_types()
                .iter()
                .all(|payload_type| profiles.get(*payload_type).is_none())
            {
                return Ok(());
            }
            return Err(Error::RecordBatchNotFound {
                payload_type: ArrowPayloadType::Profiles,
            });
        }
        if logical_arrow_bytes > result_buf.remaining() {
            return Err(Error::Dropped);
        }

        let profiles = Profiles::try_from(profiles.clone().into_raw())?;
        let records = OtapArrowRecords::Profiles(profiles);
        if estimated_reconstruction_bytes(&records, logical_arrow_bytes)? > result_buf.remaining() {
            return Err(Error::Dropped);
        }
        let request = reconstruct_profiles_request(&records)?;
        let encoded_len = request.encoded_len();
        if encoded_len > result_buf.remaining() {
            return Err(Error::Dropped);
        }
        let bytes = request.encode_to_vec();
        result_buf.extend_from_slice(&bytes)?;
        Ok(())
    }
}

fn estimated_reconstruction_bytes(
    records: &OtapArrowRecords,
    logical_arrow_bytes: usize,
) -> Result<usize> {
    const ROW_ALLOCATION_COPIES: usize = 2;
    let mut estimate = logical_arrow_bytes;

    for payload_type in Profiles::allowed_payload_types() {
        let Some(batch) = records.get(*payload_type) else {
            continue;
        };
        let row_overhead = protobuf_row_overhead(*payload_type)?
            .checked_mul(batch.num_rows())
            // Account for both the protobuf graph and temporary lookup/grouping state.
            .and_then(|value| value.checked_mul(ROW_ALLOCATION_COPIES))
            .ok_or(Error::Dropped)?;
        estimate = estimate.checked_add(row_overhead).ok_or(Error::Dropped)?;

        for (field, column) in batch.schema().fields().iter().zip(batch.columns()) {
            estimate = estimate
                .checked_add(dictionary_utf8_expansion(column)?)
                .ok_or(Error::Dropped)?;
            if field.name() == consts::ATTRIBUTE_SER {
                let values = ByteArrayAccessor::try_new(column)?;
                for row in 0..column.len() {
                    if let Some(value) = values.slice_at(row) {
                        let nested_overhead = value
                            .len()
                            .checked_mul(size_of::<AnyValue>() + size_of::<KeyValue>())
                            .ok_or(Error::Dropped)?;
                        estimate = estimate
                            .checked_add(nested_overhead)
                            .ok_or(Error::Dropped)?;
                    }
                }
            }
        }
    }

    Ok(estimate)
}

fn protobuf_row_overhead(payload_type: ArrowPayloadType) -> Result<usize> {
    let overhead = match payload_type {
        ArrowPayloadType::ResourceAttrs | ArrowPayloadType::ScopeAttrs => {
            size_of::<KeyValue>() + size_of::<AnyValue>()
        }
        ArrowPayloadType::Profiles => {
            size_of::<ResourceProfiles>()
                + size_of::<Resource>()
                + size_of::<ScopeProfiles>()
                + size_of::<InstrumentationScope>()
                + size_of::<Profile>()
        }
        ArrowPayloadType::ProfileValueTypes => size_of::<ValueType>(),
        ArrowPayloadType::Samples => size_of::<Sample>(),
        ArrowPayloadType::Stacks => size_of::<Stack>(),
        ArrowPayloadType::StackLocations => size_of::<i32>(),
        ArrowPayloadType::ProfileLocations => size_of::<Location>(),
        ArrowPayloadType::ProfileLocationLines => size_of::<Line>(),
        ArrowPayloadType::ProfileFunctions => size_of::<Function>(),
        ArrowPayloadType::ProfileMappings => size_of::<Mapping>(),
        ArrowPayloadType::ProfileLinks => size_of::<Link>(),
        ArrowPayloadType::ProfileAttrs
        | ArrowPayloadType::ProfileSampleAttrs
        | ArrowPayloadType::ProfileMappingAttrs
        | ArrowPayloadType::ProfileLocationAttrs => {
            size_of::<KeyValueAndUnit>() + size_of::<AnyValue>()
        }
        _ => {
            return Err(unexpected(
                "Profiles reconstruction estimate received an unrelated payload",
            ));
        }
    };
    Ok(overhead)
}

fn dictionary_utf8_expansion(array: &ArrayRef) -> Result<usize> {
    match array.data_type() {
        ArrowDataType::Dictionary(_, value_type) if value_type.as_ref() == &ArrowDataType::Utf8 => {
            let values = StringArrayAccessor::try_new(array)?;
            (0..array.len()).try_fold(0usize, |total, row| {
                total
                    .checked_add(values.str_at(row).map_or(0, str::len))
                    .ok_or(Error::Dropped)
            })
        }
        ArrowDataType::Struct(_) => {
            let values = array
                .as_any()
                .downcast_ref::<StructArray>()
                .ok_or_else(|| unexpected("expected StructArray during Profiles preflight"))?;
            values.columns().iter().try_fold(0usize, |total, column| {
                total
                    .checked_add(dictionary_utf8_expansion(column)?)
                    .ok_or(Error::Dropped)
            })
        }
        _ => Ok(0),
    }
}

fn reconstruct_profiles_request(
    records: &OtapArrowRecords,
) -> Result<ExportProfilesServiceRequest> {
    let root = records
        .get(ArrowPayloadType::Profiles)
        .ok_or_else(|| unexpected("Profiles root payload is missing"))?;
    let root_arrays = ProfilesRootArrays::try_from(root)?;
    let resource_ids: HashSet<u16> = (0..root.num_rows())
        .filter_map(|row| root_arrays.resource_id(row))
        .collect();
    let scope_ids: HashSet<u16> = (0..root.num_rows())
        .filter_map(|row| root_arrays.scope_id(row))
        .collect();

    let resource_attrs = records
        .get(ArrowPayloadType::ResourceAttrs)
        .map(Attribute16Arrays::try_from)
        .transpose()?
        .map(|arrays| collect_direct_attributes(arrays, &resource_ids, "resource"))
        .transpose()?
        .unwrap_or_default();
    let scope_attrs = records
        .get(ArrowPayloadType::ScopeAttrs)
        .map(Attribute16Arrays::try_from)
        .transpose()?
        .map(|arrays| collect_direct_attributes(arrays, &scope_ids, "scope"))
        .transpose()?
        .unwrap_or_default();

    let mut dictionary = DictionaryBuilder::new();
    let profile_attrs = collect_profile_attribute_indices(
        records,
        ArrowPayloadType::ProfileAttrs,
        &mut dictionary,
    )?;
    let sample_attrs = collect_profile_attribute_indices(
        records,
        ArrowPayloadType::ProfileSampleAttrs,
        &mut dictionary,
    )?;
    let mapping_attrs = collect_profile_attribute_indices(
        records,
        ArrowPayloadType::ProfileMappingAttrs,
        &mut dictionary,
    )?;
    let location_attrs = collect_profile_attribute_indices(
        records,
        ArrowPayloadType::ProfileLocationAttrs,
        &mut dictionary,
    )?;

    let mapping_indices = build_mapping_dictionary(records, &mapping_attrs, &mut dictionary)?;
    let function_indices = build_function_dictionary(records, &mut dictionary)?;
    let link_indices = build_link_dictionary(records, &mut dictionary)?;
    let location_indices = build_location_dictionary(
        records,
        &mapping_indices,
        &function_indices,
        &location_attrs,
        &mut dictionary,
    )?;
    let stack_indices = build_stack_dictionary(records, &location_indices, &mut dictionary)?;
    let value_types = collect_value_types(records, &mut dictionary)?;
    let samples = collect_samples(records, &stack_indices, &link_indices, &sample_attrs)?;

    let mut sorted_rows: Vec<usize> = (0..root.num_rows()).collect();
    sorted_rows.sort_unstable_by_key(|row| {
        (
            root_arrays.resource_id(*row),
            root_arrays.scope_id(*row),
            root_arrays.id.value(*row),
        )
    });

    let mut resource_profiles = Vec::new();
    let mut cursor = 0;
    while cursor < sorted_rows.len() {
        let first_row = sorted_rows[cursor];
        let resource_id = root_arrays.resource_id(first_row);
        let resource_start = cursor;
        while cursor < sorted_rows.len()
            && root_arrays.resource_id(sorted_rows[cursor]) == resource_id
        {
            cursor += 1;
        }
        let resource_rows = &sorted_rows[resource_start..cursor];
        let resource_schema_url = consistent_value(resource_rows, "resource.schema_url", |row| {
            root_arrays.resource_schema_url(row)
        })?;
        let resource_dropped =
            consistent_value(resource_rows, "resource.dropped_attributes_count", |row| {
                root_arrays.resource_dropped_attributes_count(row)
            })?
            .unwrap_or_default();
        let resource_attributes = resource_id
            .and_then(|id| resource_attrs.get(&id).cloned())
            .unwrap_or_default();
        let resource =
            if resource_id.is_some() || resource_dropped != 0 || !resource_attributes.is_empty() {
                Some(Resource {
                    attributes: resource_attributes,
                    dropped_attributes_count: resource_dropped,
                    entity_refs: Vec::new(),
                })
            } else {
                None
            };

        let mut scope_profiles = Vec::new();
        let mut scope_cursor = 0;
        while scope_cursor < resource_rows.len() {
            let first_scope_row = resource_rows[scope_cursor];
            let scope_id = root_arrays.scope_id(first_scope_row);
            let scope_start = scope_cursor;
            while scope_cursor < resource_rows.len()
                && root_arrays.scope_id(resource_rows[scope_cursor]) == scope_id
            {
                scope_cursor += 1;
            }
            let scope_rows = &resource_rows[scope_start..scope_cursor];
            let scope_name =
                consistent_value(scope_rows, "scope.name", |row| root_arrays.scope_name(row))?;
            let scope_version = consistent_value(scope_rows, "scope.version", |row| {
                root_arrays.scope_version(row)
            })?;
            let scope_dropped =
                consistent_value(scope_rows, "scope.dropped_attributes_count", |row| {
                    root_arrays.scope_dropped_attributes_count(row)
                })?
                .unwrap_or_default();
            let scope_attributes = scope_id
                .and_then(|id| scope_attrs.get(&id).cloned())
                .unwrap_or_default();
            let scope = if scope_id.is_some()
                || scope_name.is_some()
                || scope_version.is_some()
                || scope_dropped != 0
                || !scope_attributes.is_empty()
            {
                Some(InstrumentationScope {
                    name: scope_name.unwrap_or_default(),
                    version: scope_version.unwrap_or_default(),
                    attributes: scope_attributes,
                    dropped_attributes_count: scope_dropped,
                })
            } else {
                None
            };
            let schema_url = consistent_value(scope_rows, "schema_url", |row| {
                root_arrays
                    .schema_url
                    .as_ref()
                    .and_then(|values| normalized_string(values.value_at(row)))
            })?
            .unwrap_or_default();

            let mut profiles = Vec::with_capacity(scope_rows.len());
            for row in scope_rows {
                let profile_id = root_arrays.id.value(*row);
                let time_unix_nano = root_arrays.time_unix_nano.value(*row);
                let duration_nano = root_arrays.duration_nano.value(*row);
                _ = time_unix_nano
                    .checked_add(duration_nano)
                    .ok_or_else(|| unexpected("profile time plus duration overflows u64"))?;
                let encoded_profile_id = root_arrays
                    .profile_id
                    .as_ref()
                    .and_then(|values| values.value_at(*row))
                    .unwrap_or_default();
                if !encoded_profile_id.is_empty()
                    && encoded_profile_id.iter().all(|byte| *byte == 0)
                {
                    return Err(unexpected("profile_id is all zeroes"));
                }
                let original_payload_format = root_arrays
                    .original_payload_format
                    .as_ref()
                    .and_then(|values| values.value_at(*row))
                    .unwrap_or_default();
                let original_payload = root_arrays
                    .original_payload
                    .and_then(|values| value_large_binary(values, *row))
                    .unwrap_or_default();
                if original_payload_format.is_empty() != original_payload.is_empty() {
                    return Err(unexpected(
                        "original_payload_format and original_payload must be set together",
                    ));
                }
                profiles.push(Profile {
                    sample_type: value_types.get(&profile_id).and_then(|roles| roles[0]),
                    samples: samples.get(&profile_id).cloned().unwrap_or_default(),
                    time_unix_nano,
                    duration_nano,
                    period_type: value_types.get(&profile_id).and_then(|roles| roles[1]),
                    period: root_arrays.period.value_at(*row).unwrap_or_default(),
                    profile_id: encoded_profile_id,
                    dropped_attributes_count: root_arrays
                        .dropped_attributes_count
                        .value_at(*row)
                        .unwrap_or_default(),
                    original_payload_format,
                    original_payload,
                    attribute_indices: profile_attrs.get(&profile_id).cloned().unwrap_or_default(),
                });
            }
            scope_profiles.push(ScopeProfiles {
                scope,
                profiles,
                schema_url,
            });
        }

        resource_profiles.push(ResourceProfiles {
            resource,
            scope_profiles,
            schema_url: resource_schema_url.unwrap_or_default(),
        });
    }

    Ok(ExportProfilesServiceRequest {
        resource_profiles,
        dictionary: Some(dictionary.finish()),
    })
}

struct ProfilesRootArrays<'a> {
    id: &'a UInt32Array,
    resource_struct: Option<&'a StructArray>,
    resource: ResourceArrays<'a>,
    scope_struct: Option<&'a StructArray>,
    scope: ScopeArrays<'a>,
    schema_url: Option<StringArrayAccessor<'a>>,
    time_unix_nano: &'a UInt64Array,
    duration_nano: &'a UInt64Array,
    period: Option<&'a Int64Array>,
    profile_id: Option<ByteArrayAccessor<'a>>,
    dropped_attributes_count: Option<&'a UInt32Array>,
    original_payload_format: Option<StringArrayAccessor<'a>>,
    original_payload: Option<&'a LargeBinaryArray>,
}

impl<'a> TryFrom<&'a RecordBatch> for ProfilesRootArrays<'a> {
    type Error = Error;

    fn try_from(batch: &'a RecordBatch) -> Result<Self> {
        Ok(Self {
            id: get_u32_array(batch, consts::ID)?,
            resource_struct: optional_struct(batch, consts::RESOURCE)?,
            resource: ResourceArrays::try_from(batch)?,
            scope_struct: optional_struct(batch, consts::SCOPE)?,
            scope: ScopeArrays::try_from(batch)?,
            schema_url: optional_string(batch, consts::SCHEMA_URL)?,
            time_unix_nano: get_u64_array(batch, consts::TIME_UNIX_NANO)?,
            duration_nano: get_u64_array(batch, consts::DURATION_NANO)?,
            period: get_i64_array_opt(batch, consts::PERIOD)?,
            profile_id: batch
                .column_by_name(consts::PROFILE_ID)
                .map(ByteArrayAccessor::try_new)
                .transpose()?,
            dropped_attributes_count: get_u32_array_opt(batch, consts::DROPPED_ATTRIBUTES_COUNT)?,
            original_payload_format: optional_string(batch, consts::ORIGINAL_PAYLOAD_FORMAT)?,
            original_payload: batch
                .column_by_name(consts::ORIGINAL_PAYLOAD)
                .map(|array| {
                    array
                        .as_any()
                        .downcast_ref::<LargeBinaryArray>()
                        .ok_or_else(|| Error::ColumnDataTypeMismatch {
                            name: consts::ORIGINAL_PAYLOAD.to_string(),
                            expect: arrow::datatypes::DataType::LargeBinary,
                            actual: array.data_type().clone(),
                        })
                })
                .transpose()?,
        })
    }
}

impl ProfilesRootArrays<'_> {
    fn resource_valid(&self, row: usize) -> bool {
        self.resource_struct
            .is_some_and(|values| values.is_valid(row))
    }

    fn resource_id(&self, row: usize) -> Option<u16> {
        self.resource_valid(row)
            .then(|| self.resource.id.value_at(row))
            .flatten()
    }

    fn resource_schema_url(&self, row: usize) -> Option<String> {
        self.resource_valid(row)
            .then(|| {
                self.resource
                    .schema_url
                    .as_ref()
                    .and_then(|values| normalized_string(values.value_at(row)))
            })
            .flatten()
    }

    fn resource_dropped_attributes_count(&self, row: usize) -> Option<u32> {
        self.resource_valid(row)
            .then(|| {
                self.resource
                    .dropped_attributes_count
                    .value_at(row)
                    .filter(|value| *value != 0)
            })
            .flatten()
    }

    fn scope_valid(&self, row: usize) -> bool {
        self.scope_struct.is_some_and(|values| values.is_valid(row))
    }

    fn scope_id(&self, row: usize) -> Option<u16> {
        self.scope_valid(row)
            .then(|| self.scope.id.value_at(row))
            .flatten()
    }

    fn scope_name(&self, row: usize) -> Option<String> {
        self.scope_valid(row)
            .then(|| {
                self.scope
                    .name
                    .as_ref()
                    .and_then(|values| normalized_string(values.value_at(row)))
            })
            .flatten()
    }

    fn scope_version(&self, row: usize) -> Option<String> {
        self.scope_valid(row)
            .then(|| {
                self.scope
                    .version
                    .as_ref()
                    .and_then(|values| normalized_string(values.value_at(row)))
            })
            .flatten()
    }

    fn scope_dropped_attributes_count(&self, row: usize) -> Option<u32> {
        self.scope_valid(row)
            .then(|| {
                self.scope
                    .dropped_attributes_count
                    .value_at(row)
                    .filter(|value| *value != 0)
            })
            .flatten()
    }
}

fn optional_struct<'a>(batch: &'a RecordBatch, name: &str) -> Result<Option<&'a StructArray>> {
    batch
        .column_by_name(name)
        .map(|array| {
            array.as_any().downcast_ref::<StructArray>().ok_or_else(|| {
                Error::ColumnDataTypeMismatch {
                    name: name.to_string(),
                    expect: arrow::datatypes::DataType::Struct(arrow::datatypes::Fields::empty()),
                    actual: array.data_type().clone(),
                }
            })
        })
        .transpose()
}

fn optional_string<'a>(
    batch: &'a RecordBatch,
    name: &str,
) -> Result<Option<StringArrayAccessor<'a>>> {
    batch
        .column_by_name(name)
        .map(StringArrayAccessor::try_new)
        .transpose()
}

fn consistent_value<T, F>(rows: &[usize], field: &str, mut value_at: F) -> Result<Option<T>>
where
    T: Clone + Eq,
    F: FnMut(usize) -> Option<T>,
{
    let first = rows.first().and_then(|row| value_at(*row));
    for row in rows.iter().skip(1) {
        let value = value_at(*row);
        if value != first {
            return Err(unexpected(format!(
                "inconsistent {field} values for one Profiles group"
            )));
        }
    }
    Ok(first)
}

fn normalized_string(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

fn value_large_binary(array: &LargeBinaryArray, row: usize) -> Option<Vec<u8>> {
    array.is_valid(row).then(|| array.value(row).to_vec())
}

fn collect_direct_attributes(
    arrays: Attribute16Arrays<'_>,
    allowed_parent_ids: &HashSet<u16>,
    owner: &str,
) -> Result<BTreeMap<u16, Vec<KeyValue>>> {
    let mut result: BTreeMap<u16, Vec<KeyValue>> = BTreeMap::new();
    let mut keys: HashMap<u16, HashSet<String>> = HashMap::new();
    for row in 0..arrays.parent_id.len() {
        let parent_id = arrays
            .parent_id
            .value_at(row)
            .ok_or_else(|| unexpected("attribute row has null parent_id"))?;
        if !allowed_parent_ids.contains(&parent_id) {
            return Err(unexpected(format!(
                "{owner} attribute parent_id {parent_id} does not resolve"
            )));
        }
        let attribute = decode_key_value(&arrays, row)?;
        if attribute.key.is_empty() {
            return Err(unexpected("direct attribute key is empty"));
        }
        if !keys
            .entry(parent_id)
            .or_default()
            .insert(attribute.key.clone())
        {
            return Err(unexpected(format!(
                "duplicate direct attribute key {:?} for parent {parent_id}",
                attribute.key
            )));
        }
        result.entry(parent_id).or_default().push(attribute);
    }
    Ok(result)
}

fn decode_key_value<T: arrow::array::ArrowPrimitiveType>(
    arrays: &crate::otlp::attributes::AttributeArrays<'_, T>,
    row: usize,
) -> Result<KeyValue> {
    let key = arrays
        .attr_key
        .value_at(row)
        .ok_or_else(|| unexpected("attribute row has null key"))?;
    Ok(KeyValue {
        key,
        value: decode_any_value(&arrays.anyval_arrays, row)?,
        key_strindex: 0,
    })
}

fn decode_any_value(arrays: &AnyValueArrays<'_>, row: usize) -> Result<Option<AnyValue>> {
    let value_type = arrays
        .attr_type
        .value_at(row)
        .ok_or_else(|| unexpected("attribute row has null type"))?;
    let value_type = AttributeValueType::try_from(value_type)
        .map_err(|_| unexpected(format!("invalid attribute type {value_type}")))?;
    if value_type == AttributeValueType::Empty {
        return Ok(None);
    }
    validate_attribute_value(arrays, row, value_type)?;
    let mut buffer = ProtoBuffer::default();
    encode_any_value(arrays, row, value_type, &mut buffer)?;
    AnyValue::decode(buffer.as_ref())
        .map(Some)
        .map_err(|error| unexpected(format!("failed to decode reconstructed AnyValue: {error}")))
}

fn validate_attribute_value(
    arrays: &AnyValueArrays<'_>,
    row: usize,
    value_type: AttributeValueType,
) -> Result<()> {
    let present = match value_type {
        AttributeValueType::Empty => true,
        AttributeValueType::Str => arrays
            .attr_str
            .as_ref()
            .and_then(|values| values.value_at(row))
            .is_some(),
        AttributeValueType::Int => arrays
            .attr_int
            .as_ref()
            .and_then(|values| values.value_at(row))
            .is_some(),
        AttributeValueType::Double => arrays
            .attr_double
            .and_then(|values| values.value_at(row))
            .is_some(),
        AttributeValueType::Bool => arrays
            .attr_bool
            .and_then(|values| values.value_at(row))
            .is_some(),
        AttributeValueType::Bytes => arrays
            .attr_bytes
            .as_ref()
            .and_then(|values| values.slice_at(row))
            .is_some(),
        AttributeValueType::Map | AttributeValueType::Slice => {
            let bytes = arrays
                .attr_ser
                .as_ref()
                .and_then(|values| values.slice_at(row))
                .ok_or_else(|| unexpected("complex attribute is missing ser bytes"))?;
            let value = ciborium::from_reader::<ciborium::Value, &[u8]>(bytes)
                .map_err(|error| unexpected(format!("invalid attribute CBOR: {error}")))?;
            return match (value_type, value) {
                (AttributeValueType::Map, ciborium::Value::Map(_))
                | (AttributeValueType::Slice, ciborium::Value::Array(_)) => Ok(()),
                _ => Err(unexpected(
                    "complex attribute discriminator does not match CBOR root",
                )),
            };
        }
    };
    if present {
        Ok(())
    } else {
        Err(unexpected(format!(
            "attribute type {value_type:?} is missing its active value"
        )))
    }
}

struct ProfileAttributeArrays<'a> {
    attributes: Attribute32Arrays<'a>,
    ordinal: &'a UInt32Array,
    unit: Option<StringArrayAccessor<'a>>,
}

impl<'a> TryFrom<&'a RecordBatch> for ProfileAttributeArrays<'a> {
    type Error = Error;

    fn try_from(batch: &'a RecordBatch) -> Result<Self> {
        Ok(Self {
            attributes: Attribute32Arrays::try_from(batch)?,
            ordinal: get_u32_array(batch, consts::ORDINAL)?,
            unit: optional_string(batch, consts::UNIT)?,
        })
    }
}

fn collect_profile_attribute_indices(
    records: &OtapArrowRecords,
    payload_type: ArrowPayloadType,
    dictionary: &mut DictionaryBuilder,
) -> Result<BTreeMap<u32, Vec<i32>>> {
    let Some(batch) = records.get(payload_type) else {
        return Ok(BTreeMap::new());
    };
    let arrays = ProfileAttributeArrays::try_from(batch)?;
    let mut rows: Vec<usize> = (0..batch.num_rows()).collect();
    rows.sort_unstable_by_key(|row| {
        (
            arrays
                .attributes
                .parent_id
                .value_at(*row)
                .unwrap_or_default(),
            arrays.ordinal.value(*row),
        )
    });

    let mut result: BTreeMap<u32, Vec<i32>> = BTreeMap::new();
    let mut keys: HashMap<u32, HashSet<String>> = HashMap::new();
    for row in rows {
        let parent_id = arrays
            .attributes
            .parent_id
            .value_at(row)
            .ok_or_else(|| unexpected("Profiles attribute row has null parent_id"))?;
        let key = arrays
            .attributes
            .attr_key
            .value_at(row)
            .ok_or_else(|| unexpected("Profiles attribute row has null key"))?;
        if key.is_empty() {
            return Err(unexpected("Profiles attribute key is empty"));
        }
        if !keys.entry(parent_id).or_default().insert(key.clone()) {
            return Err(unexpected(format!(
                "duplicate Profiles attribute key {key:?} for parent {parent_id}"
            )));
        }
        let attribute = KeyValueAndUnit {
            key_strindex: dictionary.intern_string(&key)?,
            value: decode_any_value(&arrays.attributes.anyval_arrays, row)?,
            unit_strindex: arrays
                .unit
                .as_ref()
                .and_then(|unit| unit.value_at(row))
                .map(|unit| dictionary.intern_string(&unit))
                .transpose()?
                .unwrap_or_default(),
        };
        let index = dictionary.intern_attribute(attribute)?;
        result.entry(parent_id).or_default().push(index);
    }
    Ok(result)
}

struct DictionaryBuilder {
    dictionary: ProfilesDictionary,
    strings: HashMap<String, i32>,
    attributes: HashMap<Vec<u8>, i32>,
}

impl DictionaryBuilder {
    fn new() -> Self {
        let zero_link = Link {
            trace_id: vec![0; 16],
            span_id: vec![0; 8],
        };
        let dictionary = ProfilesDictionary {
            mapping_table: vec![Mapping::default()],
            location_table: vec![Location::default()],
            function_table: vec![Function::default()],
            link_table: vec![zero_link],
            string_table: vec![String::new()],
            attribute_table: vec![KeyValueAndUnit::default()],
            stack_table: vec![Stack::default()],
        };
        let mut strings = HashMap::new();
        let _ = strings.insert(String::new(), 0);
        Self {
            dictionary,
            strings,
            attributes: HashMap::new(),
        }
    }

    fn intern_string(&mut self, value: &str) -> Result<i32> {
        if let Some(index) = self.strings.get(value) {
            return Ok(*index);
        }
        let index = i32::try_from(self.dictionary.string_table.len())
            .map_err(|_| unexpected("Profiles string dictionary exceeds i32::MAX"))?;
        self.dictionary.string_table.push(value.to_string());
        let _ = self.strings.insert(value.to_string(), index);
        Ok(index)
    }

    fn intern_attribute(&mut self, attribute: KeyValueAndUnit) -> Result<i32> {
        let key = attribute.encode_to_vec();
        if let Some(index) = self.attributes.get(&key) {
            return Ok(*index);
        }
        let index = i32::try_from(self.dictionary.attribute_table.len())
            .map_err(|_| unexpected("Profiles attribute dictionary exceeds i32::MAX"))?;
        self.dictionary.attribute_table.push(attribute);
        let _ = self.attributes.insert(key, index);
        Ok(index)
    }

    fn push_mapping(&mut self, mapping: Mapping) -> Result<i32> {
        let index = dictionary_index(self.dictionary.mapping_table.len(), "mapping")?;
        self.dictionary.mapping_table.push(mapping);
        Ok(index)
    }

    fn push_function(&mut self, function: Function) -> Result<i32> {
        let index = dictionary_index(self.dictionary.function_table.len(), "function")?;
        self.dictionary.function_table.push(function);
        Ok(index)
    }

    fn push_link(&mut self, link: Link) -> Result<i32> {
        let index = dictionary_index(self.dictionary.link_table.len(), "link")?;
        self.dictionary.link_table.push(link);
        Ok(index)
    }

    fn push_location(&mut self, location: Location) -> Result<i32> {
        let index = dictionary_index(self.dictionary.location_table.len(), "location")?;
        self.dictionary.location_table.push(location);
        Ok(index)
    }

    fn push_stack(&mut self, stack: Stack) -> Result<i32> {
        let index = dictionary_index(self.dictionary.stack_table.len(), "stack")?;
        self.dictionary.stack_table.push(stack);
        Ok(index)
    }

    fn finish(self) -> ProfilesDictionary {
        self.dictionary
    }
}

fn dictionary_index(len: usize, table: &str) -> Result<i32> {
    i32::try_from(len).map_err(|_| unexpected(format!("{table} dictionary exceeds i32::MAX")))
}

fn sorted_id_rows(batch: &RecordBatch) -> Result<Vec<(u32, usize)>> {
    let ids = get_u32_array(batch, consts::ID)?;
    let mut rows: Vec<_> = ids.values().iter().copied().zip(0..ids.len()).collect();
    rows.sort_unstable_by_key(|(id, _)| *id);
    Ok(rows)
}

fn build_mapping_dictionary(
    records: &OtapArrowRecords,
    attributes: &BTreeMap<u32, Vec<i32>>,
    dictionary: &mut DictionaryBuilder,
) -> Result<HashMap<u32, i32>> {
    let Some(batch) = records.get(ArrowPayloadType::ProfileMappings) else {
        return Ok(HashMap::new());
    };
    let memory_start = get_u64_array(batch, consts::MEMORY_START)?;
    let memory_limit = get_u64_array(batch, consts::MEMORY_LIMIT)?;
    let file_offset = get_u64_array(batch, consts::FILE_OFFSET)?;
    let filename = optional_string(batch, consts::FILENAME)?;
    let mut result = HashMap::new();
    for (id, row) in sorted_id_rows(batch)? {
        let filename_value = filename
            .as_ref()
            .and_then(|values| values.value_at(row))
            .unwrap_or_default();
        let attribute_indices = attributes.get(&id).cloned().unwrap_or_default();
        let mapping = Mapping {
            memory_start: memory_start.value(row),
            memory_limit: memory_limit.value(row),
            file_offset: file_offset.value(row),
            filename_strindex: if filename_value.is_empty() {
                0
            } else {
                dictionary.intern_string(&filename_value)?
            },
            attribute_indices,
        };
        if mapping.memory_start != 0
            && mapping.memory_limit != 0
            && mapping.memory_limit < mapping.memory_start
        {
            return Err(unexpected(format!(
                "mapping {id} has an invalid address range"
            )));
        }
        if mapping == Mapping::default() {
            return Err(unexpected(format!(
                "mapping {id} is a nonzero zero-value entity"
            )));
        }
        let index = dictionary.push_mapping(mapping)?;
        let _ = result.insert(id, index);
    }
    Ok(result)
}

fn build_function_dictionary(
    records: &OtapArrowRecords,
    dictionary: &mut DictionaryBuilder,
) -> Result<HashMap<u32, i32>> {
    let Some(batch) = records.get(ArrowPayloadType::ProfileFunctions) else {
        return Ok(HashMap::new());
    };
    let name = optional_string(batch, consts::NAME)?;
    let system_name = optional_string(batch, consts::SYSTEM_NAME)?;
    let filename = optional_string(batch, consts::FILENAME)?;
    let start_line = get_i64_array_opt(batch, consts::START_LINE)?;
    let mut result = HashMap::new();
    for (id, row) in sorted_id_rows(batch)? {
        let function = Function {
            name_strindex: intern_optional_string(dictionary, &name, row)?,
            system_name_strindex: intern_optional_string(dictionary, &system_name, row)?,
            filename_strindex: intern_optional_string(dictionary, &filename, row)?,
            start_line: start_line.value_at(row).unwrap_or_default(),
        };
        if function.start_line < 0 {
            return Err(unexpected(format!(
                "function {id} has a negative start_line"
            )));
        }
        if function.name_strindex == 0
            && function.system_name_strindex == 0
            && function.filename_strindex == 0
        {
            return Err(unexpected(format!(
                "function {id} has no name, system_name, or filename"
            )));
        }
        let index = dictionary.push_function(function)?;
        let _ = result.insert(id, index);
    }
    Ok(result)
}

fn intern_optional_string(
    dictionary: &mut DictionaryBuilder,
    values: &Option<StringArrayAccessor<'_>>,
    row: usize,
) -> Result<i32> {
    values
        .as_ref()
        .and_then(|values| values.value_at(row))
        .map(|value| dictionary.intern_string(&value))
        .transpose()
        .map(Option::unwrap_or_default)
}

fn build_link_dictionary(
    records: &OtapArrowRecords,
    dictionary: &mut DictionaryBuilder,
) -> Result<HashMap<u32, i32>> {
    let Some(batch) = records.get(ArrowPayloadType::ProfileLinks) else {
        return Ok(HashMap::new());
    };
    let trace_id = ByteArrayAccessor::try_new(get_required_array(batch, consts::TRACE_ID)?)?;
    let span_id = ByteArrayAccessor::try_new(get_required_array(batch, consts::SPAN_ID)?)?;
    let mut result = HashMap::new();
    for (id, row) in sorted_id_rows(batch)? {
        let link = Link {
            trace_id: trace_id
                .value_at(row)
                .ok_or_else(|| unexpected("Profiles link has null trace_id"))?,
            span_id: span_id
                .value_at(row)
                .ok_or_else(|| unexpected("Profiles link has null span_id"))?,
        };
        validate_link_id(&link.trace_id, 16, "trace_id", id)?;
        validate_link_id(&link.span_id, 8, "span_id", id)?;
        let index = dictionary.push_link(link)?;
        let _ = result.insert(id, index);
    }
    Ok(result)
}

fn build_location_dictionary(
    records: &OtapArrowRecords,
    mapping_indices: &HashMap<u32, i32>,
    function_indices: &HashMap<u32, i32>,
    attributes: &BTreeMap<u32, Vec<i32>>,
    dictionary: &mut DictionaryBuilder,
) -> Result<HashMap<u32, i32>> {
    let lines = collect_location_lines(records, function_indices)?;
    let Some(batch) = records.get(ArrowPayloadType::ProfileLocations) else {
        return Ok(HashMap::new());
    };
    let mapping_id = get_u32_array_opt(batch, consts::MAPPING_ID)?;
    let address = get_u64_array(batch, consts::ADDRESS)?;
    let mut result = HashMap::new();
    for (id, row) in sorted_id_rows(batch)? {
        let location = Location {
            mapping_index: remap_optional_id(mapping_id.value_at(row), mapping_indices, "mapping")?,
            address: address.value(row),
            lines: lines.get(&id).cloned().unwrap_or_default(),
            attribute_indices: attributes.get(&id).cloned().unwrap_or_default(),
        };
        if location == Location::default() {
            return Err(unexpected(format!(
                "location {id} is a nonzero zero-value entity"
            )));
        }
        let index = dictionary.push_location(location)?;
        let _ = result.insert(id, index);
    }
    Ok(result)
}

fn collect_location_lines(
    records: &OtapArrowRecords,
    function_indices: &HashMap<u32, i32>,
) -> Result<BTreeMap<u32, Vec<Line>>> {
    let Some(batch) = records.get(ArrowPayloadType::ProfileLocationLines) else {
        return Ok(BTreeMap::new());
    };
    let parent_id = MaybeDictArrayAccessor::<UInt32Array>::try_new(get_required_array(
        batch,
        consts::PARENT_ID,
    )?)?;
    let ordinal = get_u32_array(batch, consts::ORDINAL)?;
    let function_id = get_u32_array_opt(batch, consts::FUNCTION_ID)?;
    let line = get_i64_array_opt(batch, consts::LINE)?
        .ok_or_else(|| unexpected("ProfileLocationLines is missing line"))?;
    let column = get_i64_array_opt(batch, consts::COLUMN)?
        .ok_or_else(|| unexpected("ProfileLocationLines is missing column"))?;
    let mut rows: Vec<usize> = (0..batch.num_rows()).collect();
    rows.sort_unstable_by_key(|row| {
        (
            parent_id.value_at(*row).unwrap_or_default(),
            ordinal.value(*row),
        )
    });
    let mut result: BTreeMap<u32, Vec<Line>> = BTreeMap::new();
    for row in rows {
        let parent = parent_id
            .value_at(row)
            .ok_or_else(|| unexpected("location line has null parent_id"))?;
        let line_value = line.value(row);
        let column_value = column.value(row);
        if line_value < 0 || column_value < 0 {
            return Err(unexpected(format!(
                "location {parent} has a negative line or column"
            )));
        }
        result.entry(parent).or_default().push(Line {
            function_index: remap_optional_id(
                function_id.value_at(row),
                function_indices,
                "function",
            )?,
            line: line_value,
            column: column_value,
        });
    }
    Ok(result)
}

fn build_stack_dictionary(
    records: &OtapArrowRecords,
    location_indices: &HashMap<u32, i32>,
    dictionary: &mut DictionaryBuilder,
) -> Result<HashMap<u32, i32>> {
    let locations = collect_stack_locations(records, location_indices)?;
    let Some(batch) = records.get(ArrowPayloadType::Stacks) else {
        return Ok(HashMap::new());
    };
    let mut result = HashMap::new();
    for (id, _) in sorted_id_rows(batch)? {
        let stack = Stack {
            location_indices: locations.get(&id).cloned().unwrap_or_default(),
        };
        if stack.location_indices.is_empty() {
            return Err(unexpected(format!(
                "stack {id} is a nonzero zero-value entity"
            )));
        }
        let index = dictionary.push_stack(stack)?;
        let _ = result.insert(id, index);
    }
    Ok(result)
}

fn collect_stack_locations(
    records: &OtapArrowRecords,
    location_indices: &HashMap<u32, i32>,
) -> Result<BTreeMap<u32, Vec<i32>>> {
    let Some(batch) = records.get(ArrowPayloadType::StackLocations) else {
        return Ok(BTreeMap::new());
    };
    let parent_id = MaybeDictArrayAccessor::<UInt32Array>::try_new(get_required_array(
        batch,
        consts::PARENT_ID,
    )?)?;
    let ordinal = get_u32_array(batch, consts::ORDINAL)?;
    let location_id = get_u32_array_opt(batch, consts::LOCATION_ID)?;
    let mut rows: Vec<usize> = (0..batch.num_rows()).collect();
    rows.sort_unstable_by_key(|row| {
        (
            parent_id.value_at(*row).unwrap_or_default(),
            ordinal.value(*row),
        )
    });
    let mut result: BTreeMap<u32, Vec<i32>> = BTreeMap::new();
    for row in rows {
        let parent = parent_id
            .value_at(row)
            .ok_or_else(|| unexpected("stack location has null parent_id"))?;
        result.entry(parent).or_default().push(remap_optional_id(
            location_id.value_at(row),
            location_indices,
            "location",
        )?);
    }
    Ok(result)
}

fn collect_value_types(
    records: &OtapArrowRecords,
    dictionary: &mut DictionaryBuilder,
) -> Result<BTreeMap<u32, [Option<ValueType>; 2]>> {
    let Some(batch) = records.get(ArrowPayloadType::ProfileValueTypes) else {
        return Ok(BTreeMap::new());
    };
    let parent_id = MaybeDictArrayAccessor::<UInt32Array>::try_new(get_required_array(
        batch,
        consts::PARENT_ID,
    )?)?;
    let role_array = get_required_array(batch, consts::ROLE)?;
    let role = role_array
        .as_any()
        .downcast_ref::<UInt8Array>()
        .ok_or_else(|| Error::ColumnDataTypeMismatch {
            name: consts::ROLE.to_string(),
            expect: arrow::datatypes::DataType::UInt8,
            actual: role_array.data_type().clone(),
        })?;
    let r#type = StringArrayAccessor::try_new(get_required_array(batch, consts::ATTRIBUTE_TYPE)?)?;
    let unit = StringArrayAccessor::try_new(get_required_array(batch, consts::UNIT)?)?;
    let mut result: BTreeMap<u32, [Option<ValueType>; 2]> = BTreeMap::new();
    for row in 0..batch.num_rows() {
        let parent = parent_id
            .value_at(row)
            .ok_or_else(|| unexpected("value type has null parent_id"))?;
        let role = role.value(row) as usize;
        let value_type = ValueType {
            type_strindex: dictionary.intern_string(&r#type.value_at(row).unwrap_or_default())?,
            unit_strindex: dictionary.intern_string(&unit.value_at(row).unwrap_or_default())?,
        };
        result.entry(parent).or_default()[role] = Some(value_type);
    }
    Ok(result)
}

fn collect_samples(
    records: &OtapArrowRecords,
    stack_indices: &HashMap<u32, i32>,
    link_indices: &HashMap<u32, i32>,
    attributes: &BTreeMap<u32, Vec<i32>>,
) -> Result<BTreeMap<u32, Vec<Sample>>> {
    let batch = records
        .get(ArrowPayloadType::Samples)
        .ok_or_else(|| unexpected("Profiles Samples payload is missing"))?;
    let id = get_u32_array(batch, consts::ID)?;
    let parent_id = MaybeDictArrayAccessor::<UInt32Array>::try_new(get_required_array(
        batch,
        consts::PARENT_ID,
    )?)?;
    let stack_id = get_u32_array_opt(batch, consts::STACK_ID)?;
    let link_id = get_u32_array_opt(batch, consts::LINK_ID)?;
    let values = required_large_list(batch, consts::VALUES)?;
    let timestamps = required_large_list(batch, consts::TIMESTAMPS_UNIX_NANO)?;
    let mut rows: Vec<usize> = (0..batch.num_rows()).collect();
    rows.sort_unstable_by_key(|row| id.value(*row));
    let mut result: BTreeMap<u32, Vec<Sample>> = BTreeMap::new();
    for row in rows {
        let sample_id = id.value(row);
        let parent = parent_id
            .value_at(row)
            .ok_or_else(|| unexpected("sample has null parent_id"))?;
        result.entry(parent).or_default().push(Sample {
            stack_index: remap_optional_id(stack_id.value_at(row), stack_indices, "stack")?,
            attribute_indices: attributes.get(&sample_id).cloned().unwrap_or_default(),
            link_index: remap_optional_id(link_id.value_at(row), link_indices, "link")?,
            values: large_list_i64_at(values, row)?,
            timestamps_unix_nano: large_list_u64_at(timestamps, row)?,
        });
    }
    Ok(result)
}

fn required_large_list<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a LargeListArray> {
    let array = get_required_array(batch, name)?;
    array
        .as_any()
        .downcast_ref::<LargeListArray>()
        .ok_or_else(|| Error::ColumnDataTypeMismatch {
            name: name.to_string(),
            expect: arrow::datatypes::DataType::LargeList(std::sync::Arc::new(
                arrow::datatypes::Field::new("item", arrow::datatypes::DataType::Null, true),
            )),
            actual: array.data_type().clone(),
        })
}

fn large_list_i64_at(array: &LargeListArray, row: usize) -> Result<Vec<i64>> {
    let values = array.value(row);
    let values = values
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| unexpected("Profiles values list does not contain Int64"))?;
    if values.null_count() != 0 {
        return Err(unexpected("Profiles values list contains null elements"));
    }
    Ok(values.values().to_vec())
}

fn large_list_u64_at(array: &LargeListArray, row: usize) -> Result<Vec<u64>> {
    let values = array.value(row);
    let values = values
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| unexpected("Profiles timestamps list does not contain UInt64"))?;
    if values.null_count() != 0 {
        return Err(unexpected(
            "Profiles timestamps list contains null elements",
        ));
    }
    Ok(values.values().to_vec())
}

fn remap_optional_id(id: Option<u32>, indexes: &HashMap<u32, i32>, entity: &str) -> Result<i32> {
    match id {
        None => Ok(0),
        Some(id) => indexes.get(&id).copied().ok_or_else(|| {
            unexpected(format!(
                "missing {entity} dictionary index for OTAP ID {id}"
            ))
        }),
    }
}

fn validate_link_id(value: &[u8], len: usize, field: &str, id: u32) -> Result<()> {
    if value.len() != len || value.iter().all(|byte| *byte == 0) {
        return Err(unexpected(format!("link {id} has an invalid {field}")));
    }
    Ok(())
}

fn unexpected(reason: impl Into<String>) -> Error {
    Error::UnexpectedRecordBatchState {
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::TryIntoWithOptions;
    use crate::encode::encode_profiles_otap_batch;
    use crate::otlp::OtlpProtoBytes;
    use crate::proto::opentelemetry::common::v1::{InstrumentationScope, any_value};
    use crate::proto::opentelemetry::profiles::v1development::{ProfilesData, ProfilesDictionary};
    use arrow::array::{ArrayRef, StringArray, UInt16Array};

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

    fn full_profiles_data() -> ProfilesData {
        let mut dictionary = zero_dictionary();
        dictionary.string_table.extend(
            [
                "cpu",
                "nanoseconds",
                "binary",
                "function",
                "attr",
                "value",
                "bytes",
            ]
            .into_iter()
            .map(str::to_string),
        );
        dictionary.attribute_table.push(KeyValueAndUnit {
            key_strindex: 5,
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValueStrindex(6)),
            }),
            unit_strindex: 7,
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
            ..Default::default()
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

        ProfilesData {
            resource_profiles: vec![ResourceProfiles {
                resource: Some(Resource {
                    attributes: vec![KeyValue {
                        key: "resource.key".to_string(),
                        value: Some(AnyValue {
                            value: Some(any_value::Value::StringValue(
                                "resource.value".to_string(),
                            )),
                        }),
                        key_strindex: 0,
                    }],
                    dropped_attributes_count: 1,
                    entity_refs: Vec::new(),
                }),
                scope_profiles: vec![ScopeProfiles {
                    scope: Some(InstrumentationScope {
                        name: "scope".to_string(),
                        version: "1.0".to_string(),
                        attributes: vec![KeyValue {
                            key: "scope.key".to_string(),
                            value: Some(AnyValue {
                                value: Some(any_value::Value::IntValue(7)),
                            }),
                            key_strindex: 0,
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
        }
    }

    /// Scenario: A complete Profiles graph is reconstructed into an OTLP request.
    /// Guarantees: All dictionary tables, envelopes, samples, and attributes are emitted.
    #[test]
    fn reconstructs_complete_profiles_request() {
        let data = full_profiles_data();
        let records = encode_profiles_otap_batch(&data).unwrap();
        let payload: OtlpProtoBytes = crate::OtapPayload::from_otap(records.clone())
            .try_into_with_default()
            .unwrap();
        let OtlpProtoBytes::ExportProfilesRequest(bytes) = payload else {
            panic!("expected Profiles bytes");
        };
        let request = ExportProfilesServiceRequest::decode(bytes).unwrap();

        assert_eq!(request.resource_profiles.len(), 1);
        assert_eq!(request.resource_profiles[0].scope_profiles.len(), 1);
        assert_eq!(
            request.resource_profiles[0].scope_profiles[0]
                .profiles
                .len(),
            1
        );
        let dictionary = request.dictionary.unwrap();
        assert_eq!(dictionary.mapping_table.len(), 2);
        assert_eq!(dictionary.location_table.len(), 2);
        assert_eq!(dictionary.function_table.len(), 2);
        assert_eq!(dictionary.link_table.len(), 2);
        assert_eq!(dictionary.stack_table.len(), 2);
        assert_eq!(dictionary.attribute_table.len(), 2);
    }

    /// Scenario: A canonical OTLP Profiles request completes both conversion directions.
    /// Guarantees: OTLP -> OTAP -> OTLP -> OTAP reaches the same canonical Arrow graph.
    #[test]
    fn profiles_round_trip_is_canonical_fixed_point() {
        let first = encode_profiles_otap_batch(&full_profiles_data()).unwrap();
        let payload: OtlpProtoBytes = crate::OtapPayload::from_otap(first.clone())
            .try_into_with_default()
            .unwrap();
        let OtlpProtoBytes::ExportProfilesRequest(bytes) = payload else {
            panic!("expected Profiles bytes");
        };
        let request = ExportProfilesServiceRequest::decode(bytes).unwrap();
        let second = encode_profiles_otap_batch(&ProfilesData {
            resource_profiles: request.resource_profiles,
            dictionary: request.dictionary,
        })
        .unwrap();

        assert_eq!(first, second);
    }

    /// Scenario: The configured OTLP output limit is smaller than the Profiles request.
    /// Guarantees: Reconstruction returns a bounded-buffer error without partial output.
    #[test]
    fn profiles_output_respects_size_limit() {
        let records = encode_profiles_otap_batch(&full_profiles_data()).unwrap();
        let payload = crate::OtapPayload::from_otap(records);
        let result: Result<OtlpProtoBytes> =
            payload.try_into_with_options(otel_arrow_dfe_config::ConversionOptions {
                otlp_size_limit: Some(std::num::NonZeroUsize::new(1).unwrap()),
            });

        assert!(matches!(result, Err(Error::Dropped)));
    }

    /// Scenario: Many profile envelopes reference one large Arrow dictionary string.
    /// Guarantees: Expanded protobuf allocation is rejected before graph reconstruction.
    #[test]
    fn profiles_output_preflights_dictionary_expansion() {
        let mut data = full_profiles_data();
        let template = data.resource_profiles.pop().unwrap();
        let shared_schema_url = "s".repeat(16 * 1024);
        for index in 0..32_u8 {
            let mut resource = template.clone();
            resource.schema_url = shared_schema_url.clone();
            resource.scope_profiles[0].schema_url = shared_schema_url.clone();
            resource.scope_profiles[0].profiles[0].profile_id = vec![index + 1; 16];
            data.resource_profiles.push(resource);
        }

        let mut records = encode_profiles_otap_batch(&data).unwrap();
        let logical_arrow_bytes = records.logical_arrow_bytes().unwrap();
        let expanded = estimated_reconstruction_bytes(&records, logical_arrow_bytes).unwrap();
        assert!(
            expanded > logical_arrow_bytes + shared_schema_url.len() * data.resource_profiles.len(),
            "preflight must account for repeated dictionary values"
        );

        let mut output =
            ProtoBuffer::with_capacity_and_limit(0, logical_arrow_bytes.saturating_add(1));
        let result = ProfilesProtoBytesEncoder::new().encode(&mut records, &mut output);
        assert!(matches!(result, Err(Error::Dropped)));
        assert!(output.is_empty());
    }

    /// Scenario: A non-empty Profiles store has child payloads but no root batch.
    /// Guarantees: Reconstruction rejects malformed data instead of emitting an empty request.
    #[test]
    fn rejects_nonempty_rootless_profiles_store() {
        let mut profiles = Profiles::default();
        profiles
            .set(
                ArrowPayloadType::Samples,
                crate::otap::testing::complete_batch(
                    ArrowPayloadType::Samples,
                    RecordBatch::new_empty(Arc::new(arrow::datatypes::Schema::empty())),
                ),
            )
            .unwrap();
        let mut records = OtapArrowRecords::Profiles(profiles);
        let mut buffer = ProtoBuffer::default();

        assert!(matches!(
            ProfilesProtoBytesEncoder::new().encode(&mut records, &mut buffer),
            Err(Error::RecordBatchNotFound {
                payload_type: ArrowPayloadType::Profiles
            })
        ));
    }

    /// Scenario: A resource attribute references an ID absent from every root row.
    /// Guarantees: Reconstruction rejects the dangling shared-attribute foreign key.
    #[test]
    fn rejects_dangling_resource_attribute_parent() {
        let records = encode_profiles_otap_batch(&full_profiles_data()).unwrap();
        let OtapArrowRecords::Profiles(mut profiles) = records else {
            panic!("expected Profiles");
        };
        profiles
            .set(
                ArrowPayloadType::ResourceAttrs,
                RecordBatch::try_from_iter([
                    (
                        consts::PARENT_ID,
                        Arc::new(UInt16Array::from(vec![999])) as ArrayRef,
                    ),
                    (
                        consts::ATTRIBUTE_KEY,
                        Arc::new(StringArray::from(vec!["key"])) as ArrayRef,
                    ),
                    (
                        consts::ATTRIBUTE_TYPE,
                        Arc::new(UInt8Array::from(vec![AttributeValueType::Str as u8])) as ArrayRef,
                    ),
                    (
                        consts::ATTRIBUTE_STR,
                        Arc::new(StringArray::from(vec!["value"])) as ArrayRef,
                    ),
                ])
                .unwrap(),
            )
            .unwrap();
        let mut records = OtapArrowRecords::Profiles(profiles);
        let mut buffer = ProtoBuffer::default();

        assert!(
            ProfilesProtoBytesEncoder::new()
                .encode(&mut records, &mut buffer)
                .is_err()
        );
    }

    /// Scenario: A Profiles attribute declares Bool but omits the Boolean value column.
    /// Guarantees: Reconstruction rejects the discriminator/value mismatch.
    #[test]
    fn rejects_missing_active_attribute_value() {
        let records = encode_profiles_otap_batch(&full_profiles_data()).unwrap();
        let OtapArrowRecords::Profiles(mut profiles) = records else {
            panic!("expected Profiles");
        };
        profiles
            .set(
                ArrowPayloadType::ProfileAttrs,
                RecordBatch::try_from_iter([
                    (
                        consts::PARENT_ID,
                        Arc::new(UInt32Array::from(vec![1])) as ArrayRef,
                    ),
                    (
                        consts::ORDINAL,
                        Arc::new(UInt32Array::from(vec![0])) as ArrayRef,
                    ),
                    (
                        consts::ATTRIBUTE_KEY,
                        Arc::new(StringArray::from(vec!["broken"])) as ArrayRef,
                    ),
                    (
                        consts::ATTRIBUTE_TYPE,
                        Arc::new(UInt8Array::from(vec![AttributeValueType::Bool as u8]))
                            as ArrayRef,
                    ),
                ])
                .unwrap(),
            )
            .unwrap();
        let mut records = OtapArrowRecords::Profiles(profiles);
        let mut buffer = ProtoBuffer::default();

        assert!(
            ProfilesProtoBytesEncoder::new()
                .encode(&mut records, &mut buffer)
                .is_err()
        );
    }
}
