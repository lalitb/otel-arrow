// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Cross-batch validation for normalized OTAP Profiles payloads.

use std::collections::{HashMap, HashSet};

use arrow::array::{
    Array, ArrayRef, FixedSizeBinaryArray, LargeListArray, RecordBatch, StringArray, UInt8Array,
    UInt32Array, UInt64Array,
};
use arrow::compute::cast;
use arrow::datatypes::DataType as ArrowDataType;

use crate::proto::opentelemetry::arrow::v1::ArrowPayloadType;
use crate::schema::consts::*;
use crate::schema::error::{Error, Result};
use crate::schema::payloads;

/// Validate schema conformance and graph invariants for one Profiles BAR.
///
/// This validator deliberately does not create a Profiles engine payload. It is
/// intended for the schema/model layer and can be called by later integration
/// code once `SignalType::Profiles` and a typed store exist.
pub fn validate_profile_batches(batches: &[(ArrowPayloadType, &RecordBatch)]) -> Result<()> {
    let mut map = HashMap::with_capacity(batches.len());

    for &(payload_type, batch) in batches {
        if !payloads::is_profile_payload_type(payload_type) {
            return invalid(format!("non-Profiles payload {payload_type:?}"));
        }
        if map.insert(payload_type, batch).is_some() {
            return invalid(format!("duplicate payload {payload_type:?}"));
        }
        payloads::get(payload_type).check_match(batch)?;
    }

    let Some(profiles) = map.get(&ArrowPayloadType::Profiles).copied() else {
        return invalid("missing PROFILES root payload");
    };

    let profile_ids = collect_ids(profiles, ArrowPayloadType::Profiles)?;
    validate_profiles_root(profiles)?;

    let sample_ids = validate_samples(&map, &profile_ids)?;
    let stack_ids = collect_optional_ids(&map, ArrowPayloadType::Stacks)?;
    let location_ids = collect_optional_ids(&map, ArrowPayloadType::ProfileLocations)?;
    let function_ids = collect_optional_ids(&map, ArrowPayloadType::ProfileFunctions)?;
    let mapping_ids = collect_optional_ids(&map, ArrowPayloadType::ProfileMappings)?;
    let link_ids = collect_optional_ids(&map, ArrowPayloadType::ProfileLinks)?;

    validate_profile_value_types(&map, &profile_ids)?;
    validate_samples_refs(&map, &profile_ids, &stack_ids, &link_ids)?;
    validate_stack_locations(&map, &stack_ids, &location_ids)?;
    validate_locations(&map, &mapping_ids)?;
    validate_location_lines(&map, &location_ids, &function_ids)?;
    validate_functions(&map)?;
    validate_mappings(&map)?;
    validate_links(&map)?;
    validate_attributes(&map, ArrowPayloadType::ProfileAttrs, &profile_ids)?;
    validate_attributes(&map, ArrowPayloadType::ProfileSampleAttrs, &sample_ids)?;
    validate_attributes(&map, ArrowPayloadType::ProfileMappingAttrs, &mapping_ids)?;
    validate_attributes(&map, ArrowPayloadType::ProfileLocationAttrs, &location_ids)?;

    Ok(())
}

fn invalid<T>(reason: impl Into<String>) -> Result<T> {
    Err(Error::InvalidProfilesGraph {
        reason: reason.into(),
    })
}

type BatchMap<'a> = HashMap<ArrowPayloadType, &'a RecordBatch>;

fn collect_optional_ids(
    map: &BatchMap<'_>,
    payload_type: ArrowPayloadType,
) -> Result<HashSet<u32>> {
    match map.get(&payload_type) {
        Some(batch) => collect_ids(batch, payload_type),
        None => Ok(HashSet::new()),
    }
}

fn collect_ids(batch: &RecordBatch, payload_type: ArrowPayloadType) -> Result<HashSet<u32>> {
    let ids = required_u32(batch, ID)?;
    let mut seen = HashSet::with_capacity(ids.len());
    for (row, id) in ids.iter().copied().enumerate() {
        let Some(id) = id else {
            return invalid(format!("{payload_type:?}.{ID} is null at row {row}"));
        };
        if id == 0 {
            return invalid(format!("{payload_type:?}.{ID} is zero at row {row}"));
        }
        if !seen.insert(id) {
            return invalid(format!("{payload_type:?}.{ID} duplicates {id}"));
        }
    }
    Ok(seen)
}

fn validate_profiles_root(batch: &RecordBatch) -> Result<()> {
    validate_profile_ids(batch)?;
    validate_original_payload_pair(batch)?;
    Ok(())
}

fn validate_profile_ids(batch: &RecordBatch) -> Result<()> {
    let Some(column) = batch.column_by_name(PROFILE_ID) else {
        return Ok(());
    };
    let ids = fixed_size_binary(column, PROFILE_ID, 16)?;
    for row in 0..ids.len() {
        if ids.is_null(row) {
            continue;
        }
        if ids.value(row).iter().all(|byte| *byte == 0) {
            return invalid(format!("{PROFILE_ID} is all zeros at row {row}"));
        }
    }
    Ok(())
}

fn validate_original_payload_pair(batch: &RecordBatch) -> Result<()> {
    let format = optional_column(batch, ORIGINAL_PAYLOAD_FORMAT);
    let payload = optional_column(batch, ORIGINAL_PAYLOAD);
    for row in 0..batch.num_rows() {
        if is_set(format, row) != is_set(payload, row) {
            return invalid(format!(
                "{ORIGINAL_PAYLOAD_FORMAT} and {ORIGINAL_PAYLOAD} must be paired at row {row}"
            ));
        }
    }
    Ok(())
}

fn validate_profile_value_types(map: &BatchMap<'_>, profile_ids: &HashSet<u32>) -> Result<()> {
    let Some(batch) = map.get(&ArrowPayloadType::ProfileValueTypes) else {
        return Ok(());
    };

    let parent_ids = required_u32(batch, PARENT_ID)?;
    let roles = required_u8(batch, ROLE)?;
    let types = optional_utf8(batch, VALUE_TYPE_TYPE)?;
    let units = optional_utf8(batch, VALUE_TYPE_UNIT)?;
    let mut seen = HashSet::with_capacity(batch.num_rows());

    for (row, parent_id) in parent_ids.iter().copied().enumerate() {
        let parent_id = required_value(parent_id, PARENT_ID, row)?;
        require_contains(profile_ids, parent_id, "PROFILE_VALUE_TYPES.parent_id", row)?;

        let role = required_value(roles[row], ROLE, row)?;
        if role > 1 {
            return invalid(format!(
                "PROFILE_VALUE_TYPES.role {role} is invalid at row {row}"
            ));
        }
        if !seen.insert((parent_id, role)) {
            return invalid(format!(
                "PROFILE_VALUE_TYPES duplicates role {role} for profile {parent_id}"
            ));
        }
        if types[row].as_deref().unwrap_or_default().is_empty()
            && units[row].as_deref().unwrap_or_default().is_empty()
        {
            return invalid(format!(
                "PROFILE_VALUE_TYPES row {row} is the canonical zero value"
            ));
        }
    }

    Ok(())
}

fn validate_samples(map: &BatchMap<'_>, profile_ids: &HashSet<u32>) -> Result<HashSet<u32>> {
    let Some(batch) = map.get(&ArrowPayloadType::Samples) else {
        return Ok(HashSet::new());
    };

    let ids = collect_ids(batch, ArrowPayloadType::Samples)?;
    let parent_ids = required_u32(batch, PARENT_ID)?;
    let values = large_list(batch, VALUES)?;
    let timestamps = large_list(batch, TIMESTAMPS_UNIX_NANO)?;

    for (row, parent_id) in parent_ids.iter().copied().enumerate() {
        let parent_id = required_value(parent_id, PARENT_ID, row)?;
        require_contains(profile_ids, parent_id, "SAMPLES.parent_id", row)?;

        let value_len = values.value_length(row);
        let timestamp_len = timestamps.value_length(row);
        if value_len == 0 && timestamp_len == 0 {
            return invalid(format!("SAMPLES row {row} has no observations"));
        }
        if value_len > 0 && timestamp_len > 0 && value_len != timestamp_len {
            return invalid(format!(
                "SAMPLES row {row} has mismatched values and timestamp lengths"
            ));
        }
    }

    Ok(ids)
}

fn validate_samples_refs(
    map: &BatchMap<'_>,
    profile_ids: &HashSet<u32>,
    stack_ids: &HashSet<u32>,
    link_ids: &HashSet<u32>,
) -> Result<()> {
    let Some(batch) = map.get(&ArrowPayloadType::Samples) else {
        return Ok(());
    };

    validate_required_refs(batch, PARENT_ID, profile_ids, "SAMPLES.parent_id")?;
    validate_optional_refs(batch, STACK_ID, stack_ids, "SAMPLES.stack_id")?;
    validate_optional_refs(batch, LINK_ID, link_ids, "SAMPLES.link_id")?;
    Ok(())
}

fn validate_stack_locations(
    map: &BatchMap<'_>,
    stack_ids: &HashSet<u32>,
    location_ids: &HashSet<u32>,
) -> Result<()> {
    let Some(batch) = map.get(&ArrowPayloadType::StackLocations) else {
        return Ok(());
    };

    let parent_ids = required_u32(batch, PARENT_ID)?;
    let ordinals = required_u32(batch, ORDINAL)?;
    let mut by_parent: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut seen = HashSet::with_capacity(batch.num_rows());

    for (row, parent_id) in parent_ids.iter().copied().enumerate() {
        let parent_id = required_value(parent_id, PARENT_ID, row)?;
        require_contains(stack_ids, parent_id, "STACK_LOCATIONS.parent_id", row)?;
        let ordinal = required_value(ordinals[row], ORDINAL, row)?;
        if !seen.insert((parent_id, ordinal)) {
            return invalid(format!(
                "STACK_LOCATIONS duplicates ordinal {ordinal} for stack {parent_id}"
            ));
        }
        by_parent.entry(parent_id).or_default().push(ordinal);
    }

    for (parent_id, mut ordinals) in by_parent {
        ordinals.sort_unstable();
        for (expected, actual) in ordinals.into_iter().enumerate() {
            if actual != expected as u32 {
                return invalid(format!(
                    "STACK_LOCATIONS ordinals for stack {parent_id} are not contiguous"
                ));
            }
        }
    }

    validate_optional_refs(
        batch,
        LOCATION_ID,
        location_ids,
        "STACK_LOCATIONS.location_id",
    )?;
    Ok(())
}

fn validate_locations(map: &BatchMap<'_>, mapping_ids: &HashSet<u32>) -> Result<()> {
    let Some(batch) = map.get(&ArrowPayloadType::ProfileLocations) else {
        return Ok(());
    };
    validate_optional_refs(
        batch,
        MAPPING_ID,
        mapping_ids,
        "PROFILE_LOCATIONS.mapping_id",
    )
}

fn validate_location_lines(
    map: &BatchMap<'_>,
    location_ids: &HashSet<u32>,
    function_ids: &HashSet<u32>,
) -> Result<()> {
    let Some(batch) = map.get(&ArrowPayloadType::ProfileLocationLines) else {
        return Ok(());
    };

    let parent_ids = required_u32(batch, PARENT_ID)?;
    let ordinals = required_u32(batch, ORDINAL)?;
    let lines = required_i64(batch, LINE)?;
    let columns = required_i64(batch, COLUMN)?;
    let mut seen = HashSet::with_capacity(batch.num_rows());

    for (row, parent_id) in parent_ids.iter().copied().enumerate() {
        let parent_id = required_value(parent_id, PARENT_ID, row)?;
        require_contains(
            location_ids,
            parent_id,
            "PROFILE_LOCATION_LINES.parent_id",
            row,
        )?;
        let ordinal = required_value(ordinals[row], ORDINAL, row)?;
        if !seen.insert((parent_id, ordinal)) {
            return invalid(format!(
                "PROFILE_LOCATION_LINES duplicates ordinal {ordinal} for location {parent_id}"
            ));
        }
        if required_value(lines[row], LINE, row)? < 0 {
            return invalid(format!(
                "PROFILE_LOCATION_LINES.line is negative at row {row}"
            ));
        }
        if required_value(columns[row], COLUMN, row)? < 0 {
            return invalid(format!(
                "PROFILE_LOCATION_LINES.column is negative at row {row}"
            ));
        }
    }

    validate_optional_refs(
        batch,
        FUNCTION_ID,
        function_ids,
        "PROFILE_LOCATION_LINES.function_id",
    )
}

fn validate_functions(map: &BatchMap<'_>) -> Result<()> {
    let Some(batch) = map.get(&ArrowPayloadType::ProfileFunctions) else {
        return Ok(());
    };

    let names = optional_utf8(batch, NAME)?;
    let system_names = optional_utf8(batch, SYSTEM_NAME)?;
    let filenames = optional_utf8(batch, FILENAME)?;

    for row in 0..batch.num_rows() {
        let has_name = !names[row].as_deref().unwrap_or_default().is_empty();
        let has_system_name = !system_names[row].as_deref().unwrap_or_default().is_empty();
        let has_filename = !filenames[row].as_deref().unwrap_or_default().is_empty();
        if !(has_name || has_system_name || has_filename) {
            return invalid(format!(
                "PROFILE_FUNCTIONS row {row} is the canonical zero value"
            ));
        }
    }

    Ok(())
}

fn validate_mappings(map: &BatchMap<'_>) -> Result<()> {
    let Some(batch) = map.get(&ArrowPayloadType::ProfileMappings) else {
        return Ok(());
    };

    let starts = required_u64(batch, MEMORY_START)?;
    let limits = required_u64(batch, MEMORY_LIMIT)?;

    for row in 0..batch.num_rows() {
        let start = required_value(starts[row], MEMORY_START, row)?;
        let limit = required_value(limits[row], MEMORY_LIMIT, row)?;
        if start != 0 && limit != 0 && limit < start {
            return invalid(format!(
                "PROFILE_MAPPINGS memory_limit is below memory_start at row {row}"
            ));
        }
    }

    Ok(())
}

fn validate_links(map: &BatchMap<'_>) -> Result<()> {
    let Some(batch) = map.get(&ArrowPayloadType::ProfileLinks) else {
        return Ok(());
    };

    for (column, len) in [(TRACE_ID, 16), (SPAN_ID, 8)] {
        let ids = fixed_size_binary(required_column(batch, column)?, column, len)?;
        for row in 0..ids.len() {
            if ids.is_null(row) {
                return invalid(format!("PROFILE_LINKS.{column} is null at row {row}"));
            }
            if ids.value(row).iter().all(|byte| *byte == 0) {
                return invalid(format!("PROFILE_LINKS.{column} is all zeros at row {row}"));
            }
        }
    }

    Ok(())
}

fn validate_attributes(
    map: &BatchMap<'_>,
    payload_type: ArrowPayloadType,
    owners: &HashSet<u32>,
) -> Result<()> {
    let Some(batch) = map.get(&payload_type) else {
        return Ok(());
    };

    let parent_ids = required_u32(batch, PARENT_ID)?;
    let ordinals = required_u32(batch, ORDINAL)?;
    let keys = optional_utf8(batch, ATTRIBUTE_KEY)?;
    let mut seen_ordinals = HashSet::with_capacity(batch.num_rows());
    let mut seen_keys = HashSet::with_capacity(batch.num_rows());

    for (row, parent_id) in parent_ids.iter().copied().enumerate() {
        let parent_id = required_value(parent_id, PARENT_ID, row)?;
        require_contains(owners, parent_id, "profile attribute parent_id", row)?;
        let ordinal = required_value(ordinals[row], ORDINAL, row)?;
        if !seen_ordinals.insert((parent_id, ordinal)) {
            return invalid(format!(
                "{payload_type:?} duplicates ordinal {ordinal} for owner {parent_id}"
            ));
        }
        let Some(key) = keys[row].as_deref() else {
            return invalid(format!("{payload_type:?}.key is null at row {row}"));
        };
        if !seen_keys.insert((parent_id, key.to_string())) {
            return invalid(format!(
                "{payload_type:?} duplicates key {key:?} for owner {parent_id}"
            ));
        }
    }

    Ok(())
}

fn validate_required_refs(
    batch: &RecordBatch,
    column: &str,
    targets: &HashSet<u32>,
    label: &str,
) -> Result<()> {
    let values = required_u32(batch, column)?;
    for (row, value) in values.into_iter().enumerate() {
        let value = required_value(value, column, row)?;
        require_contains(targets, value, label, row)?;
    }
    Ok(())
}

fn validate_optional_refs(
    batch: &RecordBatch,
    column: &str,
    targets: &HashSet<u32>,
    label: &str,
) -> Result<()> {
    let values = optional_u32(batch, column)?;
    for (row, value) in values.into_iter().enumerate() {
        if let Some(value) = value {
            require_contains(targets, value, label, row)?;
        }
    }
    Ok(())
}

fn require_contains(targets: &HashSet<u32>, value: u32, label: &str, row: usize) -> Result<()> {
    if value == 0 {
        return invalid(format!("{label} is zero at row {row}"));
    }
    if !targets.contains(&value) {
        return invalid(format!(
            "{label} references missing id {value} at row {row}"
        ));
    }
    Ok(())
}

fn required_value<T: Copy>(value: Option<T>, column: &str, row: usize) -> Result<T> {
    value.ok_or_else(|| Error::InvalidProfilesGraph {
        reason: format!("{column} is null at row {row}"),
    })
}

fn optional_column<'a>(batch: &'a RecordBatch, column: &str) -> Option<&'a ArrayRef> {
    batch.column_by_name(column)
}

fn required_column<'a>(batch: &'a RecordBatch, column: &str) -> Result<&'a ArrayRef> {
    batch
        .column_by_name(column)
        .ok_or_else(|| Error::InvalidProfilesGraph {
            reason: format!("missing column {column}"),
        })
}

fn is_set(column: Option<&ArrayRef>, row: usize) -> bool {
    column.is_some_and(|array| !array.is_null(row))
}

fn required_u32(batch: &RecordBatch, column: &str) -> Result<Vec<Option<u32>>> {
    numeric_values(batch, column, &ArrowDataType::UInt32)
}

fn optional_u32(batch: &RecordBatch, column: &str) -> Result<Vec<Option<u32>>> {
    optional_numeric_values(batch, column, &ArrowDataType::UInt32)
}

fn required_u8(batch: &RecordBatch, column: &str) -> Result<Vec<Option<u8>>> {
    numeric_values(batch, column, &ArrowDataType::UInt8)
}

fn required_u64(batch: &RecordBatch, column: &str) -> Result<Vec<Option<u64>>> {
    numeric_values(batch, column, &ArrowDataType::UInt64)
}

fn required_i64(batch: &RecordBatch, column: &str) -> Result<Vec<Option<i64>>> {
    numeric_values(batch, column, &ArrowDataType::Int64)
}

fn optional_numeric_values<T>(
    batch: &RecordBatch,
    column: &str,
    data_type: &ArrowDataType,
) -> Result<Vec<Option<T>>>
where
    T: ArrowNativeValue,
{
    match batch.column_by_name(column) {
        Some(_) => numeric_values(batch, column, data_type),
        None => Ok(vec![None; batch.num_rows()]),
    }
}

trait ArrowNativeValue: Copy + 'static {
    fn values(array: &ArrayRef) -> Result<Vec<Option<Self>>>;
}

impl ArrowNativeValue for u8 {
    fn values(array: &ArrayRef) -> Result<Vec<Option<Self>>> {
        let array = array
            .as_any()
            .downcast_ref::<UInt8Array>()
            .expect("cast should produce UInt8Array");
        Ok((0..array.len())
            .map(|row| (!array.is_null(row)).then(|| array.value(row)))
            .collect())
    }
}

impl ArrowNativeValue for u32 {
    fn values(array: &ArrayRef) -> Result<Vec<Option<Self>>> {
        let array = array
            .as_any()
            .downcast_ref::<UInt32Array>()
            .expect("cast should produce UInt32Array");
        Ok((0..array.len())
            .map(|row| (!array.is_null(row)).then(|| array.value(row)))
            .collect())
    }
}

impl ArrowNativeValue for u64 {
    fn values(array: &ArrayRef) -> Result<Vec<Option<Self>>> {
        let array = array
            .as_any()
            .downcast_ref::<UInt64Array>()
            .expect("cast should produce UInt64Array");
        Ok((0..array.len())
            .map(|row| (!array.is_null(row)).then(|| array.value(row)))
            .collect())
    }
}

impl ArrowNativeValue for i64 {
    fn values(array: &ArrayRef) -> Result<Vec<Option<Self>>> {
        let array = array
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .expect("cast should produce Int64Array");
        Ok((0..array.len())
            .map(|row| (!array.is_null(row)).then(|| array.value(row)))
            .collect())
    }
}

fn numeric_values<T>(
    batch: &RecordBatch,
    column: &str,
    data_type: &ArrowDataType,
) -> Result<Vec<Option<T>>>
where
    T: ArrowNativeValue,
{
    let array = required_column(batch, column)?;
    let casted = cast(array.as_ref(), data_type).map_err(|source| Error::InvalidProfilesGraph {
        reason: format!("failed to read numeric column {column}: {source}"),
    })?;
    T::values(&casted)
}

fn optional_utf8(batch: &RecordBatch, column: &str) -> Result<Vec<Option<String>>> {
    let Some(array) = batch.column_by_name(column) else {
        return Ok(vec![None; batch.num_rows()]);
    };
    let casted = cast(array.as_ref(), &ArrowDataType::Utf8).map_err(|source| {
        Error::InvalidProfilesGraph {
            reason: format!("failed to read string column {column}: {source}"),
        }
    })?;
    let strings = casted
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("cast should produce StringArray");
    Ok((0..strings.len())
        .map(|row| (!strings.is_null(row)).then(|| strings.value(row).to_string()))
        .collect())
}

fn large_list<'a>(batch: &'a RecordBatch, column: &str) -> Result<&'a LargeListArray> {
    required_column(batch, column)?
        .as_any()
        .downcast_ref::<LargeListArray>()
        .ok_or_else(|| Error::InvalidProfilesGraph {
            reason: format!("{column} must be LargeList"),
        })
}

fn fixed_size_binary<'a>(
    array: &'a ArrayRef,
    column: &str,
    len: i32,
) -> Result<&'a FixedSizeBinaryArray> {
    let array = array
        .as_any()
        .downcast_ref::<FixedSizeBinaryArray>()
        .ok_or_else(|| Error::InvalidProfilesGraph {
            reason: format!("{column} must be FixedSizeBinary({len})"),
        })?;
    if array.value_length() != len {
        return invalid(format!("{column} must be FixedSizeBinary({len})"));
    }
    Ok(array)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use arrow::array::{
        BinaryArray, BooleanArray, Float64Array, Int64Array, LargeBinaryArray, LargeListArray,
        StringArray, UInt8Array,
    };
    use arrow::datatypes::{
        DataType as ArrowDataType, Field as ArrowField, Int64Type, Schema as ArrowSchema,
        UInt64Type,
    };

    fn batch(fields: Vec<ArrowField>, arrays: Vec<ArrayRef>) -> RecordBatch {
        RecordBatch::try_new(Arc::new(ArrowSchema::new(fields)), arrays).unwrap()
    }

    fn profiles_batch() -> RecordBatch {
        batch(
            vec![
                ArrowField::new(ID, ArrowDataType::UInt32, false),
                ArrowField::new(TIME_UNIX_NANO, ArrowDataType::UInt64, false),
                ArrowField::new(DURATION_NANO, ArrowDataType::UInt64, false),
            ],
            vec![
                Arc::new(UInt32Array::from(vec![1])) as ArrayRef,
                Arc::new(UInt64Array::from(vec![100])) as ArrayRef,
                Arc::new(UInt64Array::from(vec![10])) as ArrayRef,
            ],
        )
    }

    fn samples_batch(stack_id: Option<u32>, values: Vec<i64>, timestamps: Vec<u64>) -> RecordBatch {
        batch(
            vec![
                ArrowField::new(ID, ArrowDataType::UInt32, false),
                ArrowField::new(PARENT_ID, ArrowDataType::UInt32, false),
                ArrowField::new(STACK_ID, ArrowDataType::UInt32, true),
                ArrowField::new(LINK_ID, ArrowDataType::UInt32, true),
                ArrowField::new(
                    VALUES,
                    ArrowDataType::LargeList(Arc::new(ArrowField::new(
                        "item",
                        ArrowDataType::Int64,
                        true,
                    ))),
                    false,
                ),
                ArrowField::new(
                    TIMESTAMPS_UNIX_NANO,
                    ArrowDataType::LargeList(Arc::new(ArrowField::new(
                        "item",
                        ArrowDataType::UInt64,
                        true,
                    ))),
                    false,
                ),
            ],
            vec![
                Arc::new(UInt32Array::from(vec![10])) as ArrayRef,
                Arc::new(UInt32Array::from(vec![1])) as ArrayRef,
                Arc::new(UInt32Array::from(vec![stack_id])) as ArrayRef,
                Arc::new(UInt32Array::from(vec![Some(40)])) as ArrayRef,
                Arc::new(large_i64_list(values)) as ArrayRef,
                Arc::new(large_u64_list(timestamps)) as ArrayRef,
            ],
        )
    }

    fn large_i64_list(values: Vec<i64>) -> LargeListArray {
        LargeListArray::from_iter_primitive::<Int64Type, _, _>(vec![Some(
            values.into_iter().map(Some).collect::<Vec<_>>(),
        )])
    }

    fn large_u64_list(values: Vec<u64>) -> LargeListArray {
        LargeListArray::from_iter_primitive::<UInt64Type, _, _>(vec![Some(
            values.into_iter().map(Some).collect::<Vec<_>>(),
        )])
    }

    fn stacks_batch() -> RecordBatch {
        one_u32_id_batch(20)
    }

    fn locations_batch() -> RecordBatch {
        batch(
            vec![
                ArrowField::new(ID, ArrowDataType::UInt32, false),
                ArrowField::new(MAPPING_ID, ArrowDataType::UInt32, true),
                ArrowField::new(ADDRESS, ArrowDataType::UInt64, false),
            ],
            vec![
                Arc::new(UInt32Array::from(vec![30])) as ArrayRef,
                Arc::new(UInt32Array::from(vec![Some(50)])) as ArrayRef,
                Arc::new(UInt64Array::from(vec![1])) as ArrayRef,
            ],
        )
    }

    fn stack_locations_batch(location_id: Option<u32>) -> RecordBatch {
        parent_ordinal_ref_batch(20, location_id, LOCATION_ID)
    }

    fn lines_batch() -> RecordBatch {
        batch(
            vec![
                ArrowField::new(PARENT_ID, ArrowDataType::UInt32, false),
                ArrowField::new(ORDINAL, ArrowDataType::UInt32, false),
                ArrowField::new(FUNCTION_ID, ArrowDataType::UInt32, true),
                ArrowField::new(LINE, ArrowDataType::Int64, false),
                ArrowField::new(COLUMN, ArrowDataType::Int64, false),
            ],
            vec![
                Arc::new(UInt32Array::from(vec![30])) as ArrayRef,
                Arc::new(UInt32Array::from(vec![0])) as ArrayRef,
                Arc::new(UInt32Array::from(vec![Some(60)])) as ArrayRef,
                Arc::new(Int64Array::from(vec![1])) as ArrayRef,
                Arc::new(Int64Array::from(vec![1])) as ArrayRef,
            ],
        )
    }

    fn functions_batch() -> RecordBatch {
        batch(
            vec![
                ArrowField::new(ID, ArrowDataType::UInt32, false),
                ArrowField::new(NAME, ArrowDataType::Utf8, true),
            ],
            vec![
                Arc::new(UInt32Array::from(vec![60])) as ArrayRef,
                Arc::new(StringArray::from(vec![Some("main")])) as ArrayRef,
            ],
        )
    }

    fn mappings_batch() -> RecordBatch {
        batch(
            vec![
                ArrowField::new(ID, ArrowDataType::UInt32, false),
                ArrowField::new(MEMORY_START, ArrowDataType::UInt64, false),
                ArrowField::new(MEMORY_LIMIT, ArrowDataType::UInt64, false),
                ArrowField::new(FILE_OFFSET, ArrowDataType::UInt64, false),
            ],
            vec![
                Arc::new(UInt32Array::from(vec![50])) as ArrayRef,
                Arc::new(UInt64Array::from(vec![1])) as ArrayRef,
                Arc::new(UInt64Array::from(vec![2])) as ArrayRef,
                Arc::new(UInt64Array::from(vec![0])) as ArrayRef,
            ],
        )
    }

    fn links_batch() -> RecordBatch {
        batch(
            vec![
                ArrowField::new(ID, ArrowDataType::UInt32, false),
                ArrowField::new(TRACE_ID, ArrowDataType::FixedSizeBinary(16), false),
                ArrowField::new(SPAN_ID, ArrowDataType::FixedSizeBinary(8), false),
            ],
            vec![
                Arc::new(UInt32Array::from(vec![40])) as ArrayRef,
                Arc::new(
                    FixedSizeBinaryArray::try_from_iter([[1u8; 16].as_slice()].into_iter())
                        .unwrap(),
                ) as ArrayRef,
                Arc::new(
                    FixedSizeBinaryArray::try_from_iter([[2u8; 8].as_slice()].into_iter()).unwrap(),
                ) as ArrayRef,
            ],
        )
    }

    fn value_types_batch() -> RecordBatch {
        batch(
            vec![
                ArrowField::new(PARENT_ID, ArrowDataType::UInt32, false),
                ArrowField::new(ROLE, ArrowDataType::UInt8, false),
                ArrowField::new(VALUE_TYPE_TYPE, ArrowDataType::Utf8, false),
                ArrowField::new(VALUE_TYPE_UNIT, ArrowDataType::Utf8, false),
            ],
            vec![
                Arc::new(UInt32Array::from(vec![1])) as ArrayRef,
                Arc::new(UInt8Array::from(vec![0])) as ArrayRef,
                Arc::new(StringArray::from(vec!["cpu"])) as ArrayRef,
                Arc::new(StringArray::from(vec!["ns"])) as ArrayRef,
            ],
        )
    }

    fn sample_attrs_batch() -> RecordBatch {
        batch(
            vec![
                ArrowField::new(PARENT_ID, ArrowDataType::UInt32, false),
                ArrowField::new(ORDINAL, ArrowDataType::UInt32, false),
                ArrowField::new(ATTRIBUTE_KEY, ArrowDataType::Utf8, false),
                ArrowField::new(ATTRIBUTE_TYPE, ArrowDataType::UInt8, false),
                ArrowField::new(ATTRIBUTE_STR, ArrowDataType::Utf8, true),
                ArrowField::new(ATTRIBUTE_INT, ArrowDataType::Int64, true),
                ArrowField::new(ATTRIBUTE_DOUBLE, ArrowDataType::Float64, true),
                ArrowField::new(ATTRIBUTE_BOOL, ArrowDataType::Boolean, true),
                ArrowField::new(ATTRIBUTE_BYTES, ArrowDataType::Binary, true),
                ArrowField::new(ATTRIBUTE_SER, ArrowDataType::Binary, true),
                ArrowField::new(ATTRIBUTE_UNIT, ArrowDataType::Utf8, true),
            ],
            vec![
                Arc::new(UInt32Array::from(vec![10])) as ArrayRef,
                Arc::new(UInt32Array::from(vec![0])) as ArrayRef,
                Arc::new(StringArray::from(vec!["thread.name"])) as ArrayRef,
                Arc::new(UInt8Array::from(vec![0])) as ArrayRef,
                Arc::new(StringArray::from(vec![Some("worker")])) as ArrayRef,
                Arc::new(Int64Array::from(vec![None::<i64>])) as ArrayRef,
                Arc::new(Float64Array::from(vec![None::<f64>])) as ArrayRef,
                Arc::new(BooleanArray::from(vec![None::<bool>])) as ArrayRef,
                Arc::new(BinaryArray::from(vec![None::<&[u8]>])) as ArrayRef,
                Arc::new(BinaryArray::from(vec![None::<&[u8]>])) as ArrayRef,
                Arc::new(StringArray::from(vec![None::<&str>])) as ArrayRef,
            ],
        )
    }

    fn one_u32_id_batch(id: u32) -> RecordBatch {
        batch(
            vec![ArrowField::new(ID, ArrowDataType::UInt32, false)],
            vec![Arc::new(UInt32Array::from(vec![id])) as ArrayRef],
        )
    }

    fn parent_ordinal_ref_batch(parent_id: u32, ref_id: Option<u32>, ref_col: &str) -> RecordBatch {
        batch(
            vec![
                ArrowField::new(PARENT_ID, ArrowDataType::UInt32, false),
                ArrowField::new(ORDINAL, ArrowDataType::UInt32, false),
                ArrowField::new(ref_col, ArrowDataType::UInt32, true),
            ],
            vec![
                Arc::new(UInt32Array::from(vec![parent_id])) as ArrayRef,
                Arc::new(UInt32Array::from(vec![0])) as ArrayRef,
                Arc::new(UInt32Array::from(vec![ref_id])) as ArrayRef,
            ],
        )
    }

    fn valid_batches(samples: RecordBatch) -> Vec<(ArrowPayloadType, RecordBatch)> {
        vec![
            (ArrowPayloadType::Profiles, profiles_batch()),
            (ArrowPayloadType::ProfileValueTypes, value_types_batch()),
            (ArrowPayloadType::Samples, samples),
            (ArrowPayloadType::Stacks, stacks_batch()),
            (
                ArrowPayloadType::StackLocations,
                stack_locations_batch(Some(30)),
            ),
            (ArrowPayloadType::ProfileLocations, locations_batch()),
            (ArrowPayloadType::ProfileLocationLines, lines_batch()),
            (ArrowPayloadType::ProfileFunctions, functions_batch()),
            (ArrowPayloadType::ProfileMappings, mappings_batch()),
            (ArrowPayloadType::ProfileLinks, links_batch()),
            (ArrowPayloadType::ProfileSampleAttrs, sample_attrs_batch()),
        ]
    }

    fn refs(batches: &[(ArrowPayloadType, RecordBatch)]) -> Vec<(ArrowPayloadType, &RecordBatch)> {
        batches
            .iter()
            .map(|(payload_type, batch)| (*payload_type, batch))
            .collect()
    }

    /// Scenario: A minimal normalized Profiles graph includes every shared entity table.
    /// Guarantees: Schema checks and graph references accept a valid Profiles BAR.
    #[test]
    fn validates_minimal_profiles_graph() {
        let batches = valid_batches(samples_batch(Some(20), vec![1], vec![100]));

        validate_profile_batches(&refs(&batches)).unwrap();
    }

    /// Scenario: A sample points at a stack ID that does not exist in STACKS.
    /// Guarantees: Dangling Profiles foreign keys are rejected before engine use.
    #[test]
    fn rejects_dangling_sample_stack_reference() {
        let batches = valid_batches(samples_batch(Some(99), vec![1], vec![100]));

        assert!(validate_profile_batches(&refs(&batches)).is_err());
    }

    /// Scenario: Two stack-location rows use the same parent stack and ordinal.
    /// Guarantees: Ordered child relationships cannot contain ambiguous positions.
    #[test]
    fn rejects_duplicate_stack_location_ordinals() {
        let mut batches = valid_batches(samples_batch(Some(20), vec![1], vec![100]));
        batches.retain(|(payload_type, _)| *payload_type != ArrowPayloadType::StackLocations);
        batches.push((
            ArrowPayloadType::StackLocations,
            batch(
                vec![
                    ArrowField::new(PARENT_ID, ArrowDataType::UInt32, false),
                    ArrowField::new(ORDINAL, ArrowDataType::UInt32, false),
                    ArrowField::new(LOCATION_ID, ArrowDataType::UInt32, true),
                ],
                vec![
                    Arc::new(UInt32Array::from(vec![20, 20])) as ArrayRef,
                    Arc::new(UInt32Array::from(vec![0, 0])) as ArrayRef,
                    Arc::new(UInt32Array::from(vec![Some(30), Some(30)])) as ArrayRef,
                ],
            ),
        ));

        assert!(validate_profile_batches(&refs(&batches)).is_err());
    }

    /// Scenario: A sample has populated values and timestamps with different lengths.
    /// Guarantees: Observation lists keep value/timestamp event alignment.
    #[test]
    fn rejects_mismatched_sample_observation_lengths() {
        let batches = valid_batches(samples_batch(Some(20), vec![1, 2], vec![100]));

        assert!(validate_profile_batches(&refs(&batches)).is_err());
    }

    /// Scenario: A profile sets original payload bytes without its format string.
    /// Guarantees: Original payload fields are emitted and consumed as a pair.
    #[test]
    fn rejects_unpaired_original_payload() {
        let mut batches = valid_batches(samples_batch(Some(20), vec![1], vec![100]));
        batches.retain(|(payload_type, _)| *payload_type != ArrowPayloadType::Profiles);
        batches.push((
            ArrowPayloadType::Profiles,
            batch(
                vec![
                    ArrowField::new(ID, ArrowDataType::UInt32, false),
                    ArrowField::new(TIME_UNIX_NANO, ArrowDataType::UInt64, false),
                    ArrowField::new(DURATION_NANO, ArrowDataType::UInt64, false),
                    ArrowField::new(ORIGINAL_PAYLOAD, ArrowDataType::LargeBinary, true),
                ],
                vec![
                    Arc::new(UInt32Array::from(vec![1])) as ArrayRef,
                    Arc::new(UInt64Array::from(vec![100])) as ArrayRef,
                    Arc::new(UInt64Array::from(vec![10])) as ArrayRef,
                    Arc::new(LargeBinaryArray::from(vec![Some(b"pprof".as_slice())])) as ArrayRef,
                ],
            ),
        ));

        assert!(validate_profile_batches(&refs(&batches)).is_err());
    }
}
