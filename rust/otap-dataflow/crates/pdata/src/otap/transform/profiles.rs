// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Bounded, fail-atomic transformations for OTAP Profiles graphs.

use std::collections::{HashMap, HashSet};
use std::mem::size_of;
use std::sync::Arc;

use arrow::array::{ArrayRef, BooleanArray, BooleanBuilder, RecordBatch, StringArray, UInt32Array};
use arrow::compute::{concat_batches, filter_record_batch, take};
use arrow::datatypes::{DataType, Schema};
use otel_arrow_dfe_config::SignalType;
use serde::{Deserialize, Serialize};

use crate::arrays::{
    NullableArrayAccessor, StringArrayAccessor, UInt32ArrayAccessor, get_required_array,
    get_u32_array,
};
use crate::error::{Error, Result};
use crate::otap::{OtapArrowRecords, Profiles};
use crate::proto::opentelemetry::arrow::v1::ArrowPayloadType;
use crate::schema::consts::{self, metadata};

use super::{AttributesTransform, TransformStats, transform_attributes_with_stats};

const DEFAULT_MAX_OUTPUT_ROWS: usize = 1_000_000;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 256 * 1024 * 1024;
const DEFAULT_MAX_CLONED_ROWS: usize = 100_000;

/// Output limits applied before a transformed Profiles graph is published.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfilesTransformLimits {
    /// Maximum total rows across every Profiles payload in one BAR.
    pub max_output_rows: usize,
    /// Maximum retained Arrow bytes across every Profiles payload in one BAR.
    pub max_output_bytes: usize,
    /// Maximum rows cloned by one selection-local copy-on-write operation.
    pub max_cloned_rows: usize,
}

impl Default for ProfilesTransformLimits {
    fn default() -> Self {
        Self {
            max_output_rows: DEFAULT_MAX_OUTPUT_ROWS,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            max_cloned_rows: DEFAULT_MAX_CLONED_ROWS,
        }
    }
}

/// Result counters for sample filtering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProfilesFilterStats {
    /// Samples present before filtering.
    pub samples_before: u64,
    /// Samples retained after filtering.
    pub samples_after: u64,
}

impl ProfilesFilterStats {
    /// Samples removed by the filter.
    #[must_use]
    pub const fn samples_dropped(self) -> u64 {
        self.samples_before.saturating_sub(self.samples_after)
    }
}

/// Compaction behavior.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfilesCompactionOptions {
    /// Rewrite all Profiles entity IDs into deterministic dense ranges.
    pub dense_ids: bool,
}

/// Result counters for graph compaction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProfilesCompactionStats {
    /// Rows present before compaction.
    pub rows_before: u64,
    /// Rows retained after compaction.
    pub rows_after: u64,
    /// ID and foreign-key values changed by dense remapping.
    pub rewritten_ids: u64,
}

/// Result counters for function-filename redaction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProfilesRedactionStats {
    /// Function filename values replaced.
    pub functions_redacted: u64,
    /// Graph rows cloned for selection-local copy-on-write.
    pub cloned_rows: u64,
}

/// Profile attribute owner whose complete BAR-scoped row set is transformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfilesAttributeOwner {
    /// Attributes owned directly by profile roots.
    Profile,
    /// Attributes owned directly by samples.
    Sample,
    /// Attributes owned by globally shared mappings.
    Mapping,
    /// Attributes owned by globally shared locations.
    Location,
}

impl ProfilesAttributeOwner {
    const fn payload_type(self) -> ArrowPayloadType {
        match self {
            Self::Profile => ArrowPayloadType::ProfileAttrs,
            Self::Sample => ArrowPayloadType::ProfileSampleAttrs,
            Self::Mapping => ArrowPayloadType::ProfileMappingAttrs,
            Self::Location => ArrowPayloadType::ProfileLocationAttrs,
        }
    }

    const fn parent_payload_type(self) -> ArrowPayloadType {
        match self {
            Self::Profile => ArrowPayloadType::Profiles,
            Self::Sample => ArrowPayloadType::Samples,
            Self::Mapping => ArrowPayloadType::ProfileMappings,
            Self::Location => ArrowPayloadType::ProfileLocations,
        }
    }
}

/// Filter sample rows with a sample-aligned selection vector.
///
/// Dimension rows are intentionally retained as possible orphans. Call
/// [`compact_profile_dimensions`] explicitly when graph compaction is desired.
pub fn filter_profile_samples(
    records: &OtapArrowRecords,
    selection: &BooleanArray,
    limits: ProfilesTransformLimits,
) -> Result<(OtapArrowRecords, ProfilesFilterStats)> {
    let mut candidate = clone_profiles(records)?;
    let samples = required_batch(records, ArrowPayloadType::Samples)?;
    if selection.len() != samples.num_rows() {
        return Err(Error::InvalidProfilesSelectionLength {
            expected: samples.num_rows(),
            actual: selection.len(),
        });
    }

    let filtered_samples = filter_record_batch(samples, selection)
        .map_err(|source| Error::ColumnLengthMismatch { source })?;
    let retained_sample_ids = entity_ids(&filtered_samples, consts::ID)?;
    candidate.set(ArrowPayloadType::Samples, filtered_samples)?;

    if let Some(sample_attrs) = records.get(ArrowPayloadType::ProfileSampleAttrs) {
        let filtered_attrs =
            filter_batch_by_ids(sample_attrs, consts::PARENT_ID, &retained_sample_ids)?;
        if filtered_attrs.num_rows() == 0 {
            candidate.remove(ArrowPayloadType::ProfileSampleAttrs);
        } else {
            candidate.set(
                ArrowPayloadType::ProfileSampleAttrs,
                normalize_attribute_ordinals(filtered_attrs)?,
            )?;
        }
    }

    let output = finalize(candidate, limits)?;
    let samples_after = output
        .get(ArrowPayloadType::Samples)
        .map_or(0, RecordBatch::num_rows);
    Ok((
        output,
        ProfilesFilterStats {
            samples_before: u64::try_from(samples.num_rows()).unwrap_or(u64::MAX),
            samples_after: u64::try_from(samples_after).unwrap_or(u64::MAX),
        },
    ))
}

/// Remove dimension rows that are unreachable from retained samples.
///
/// Profiles, samples, value types, profile attributes, and sample attributes
/// remain present. Shared stacks, locations, mappings, functions, links, and
/// their owned attributes are retained only when reachable.
pub fn compact_profile_dimensions(
    records: &OtapArrowRecords,
    options: ProfilesCompactionOptions,
    limits: ProfilesTransformLimits,
) -> Result<(OtapArrowRecords, ProfilesCompactionStats)> {
    let mut candidate = clone_profiles(records)?;
    let rows_before = total_rows(records)?;
    let samples = required_batch(records, ArrowPayloadType::Samples)?;

    let reachable_stacks = referenced_ids(samples, consts::STACK_ID)?;
    let reachable_links = referenced_ids(samples, consts::LINK_ID)?;

    retain_optional_rows(
        &mut candidate,
        ArrowPayloadType::Stacks,
        consts::ID,
        &reachable_stacks,
    )?;
    retain_optional_rows(
        &mut candidate,
        ArrowPayloadType::ProfileLinks,
        consts::ID,
        &reachable_links,
    )?;
    retain_optional_rows(
        &mut candidate,
        ArrowPayloadType::StackLocations,
        consts::PARENT_ID,
        &reachable_stacks,
    )?;

    let reachable_locations = candidate
        .get(ArrowPayloadType::StackLocations)
        .map(|batch| referenced_ids(batch, consts::LOCATION_ID))
        .transpose()?
        .unwrap_or_default();
    retain_optional_rows(
        &mut candidate,
        ArrowPayloadType::ProfileLocations,
        consts::ID,
        &reachable_locations,
    )?;
    retain_optional_rows(
        &mut candidate,
        ArrowPayloadType::ProfileLocationLines,
        consts::PARENT_ID,
        &reachable_locations,
    )?;
    retain_optional_rows(
        &mut candidate,
        ArrowPayloadType::ProfileLocationAttrs,
        consts::PARENT_ID,
        &reachable_locations,
    )?;

    let reachable_mappings = candidate
        .get(ArrowPayloadType::ProfileLocations)
        .map(|batch| referenced_ids(batch, consts::MAPPING_ID))
        .transpose()?
        .unwrap_or_default();
    let reachable_functions = candidate
        .get(ArrowPayloadType::ProfileLocationLines)
        .map(|batch| referenced_ids(batch, consts::FUNCTION_ID))
        .transpose()?
        .unwrap_or_default();

    retain_optional_rows(
        &mut candidate,
        ArrowPayloadType::ProfileMappings,
        consts::ID,
        &reachable_mappings,
    )?;
    retain_optional_rows(
        &mut candidate,
        ArrowPayloadType::ProfileMappingAttrs,
        consts::PARENT_ID,
        &reachable_mappings,
    )?;
    retain_optional_rows(
        &mut candidate,
        ArrowPayloadType::ProfileFunctions,
        consts::ID,
        &reachable_functions,
    )?;

    let rewritten_ids = if options.dense_ids {
        remap_dense_ids(&mut candidate)?
    } else {
        0
    };

    let output = finalize(candidate, limits)?;
    let rows_after = total_rows(&output)?;
    Ok((
        output,
        ProfilesCompactionStats {
            rows_before: u64::try_from(rows_before).unwrap_or(u64::MAX),
            rows_after: u64::try_from(rows_after).unwrap_or(u64::MAX),
            rewritten_ids,
        },
    ))
}

/// Transform every attribute row for one explicit Profiles owner category.
///
/// Mapping and location operations are global to those shared BAR-scoped
/// owners. Selection-local shared-entity mutation requires copy-on-write and is
/// intentionally not implied by this API.
pub fn transform_profile_attributes(
    records: &OtapArrowRecords,
    owner: ProfilesAttributeOwner,
    transform: &AttributesTransform,
    limits: ProfilesTransformLimits,
) -> Result<(OtapArrowRecords, TransformStats)> {
    validate_attribute_transform_support(transform)?;

    enforce_limits(records, limits)?;
    let mut candidate = clone_profiles(records)?;
    let payload_type = owner.payload_type();
    let Some(attrs) = records.get(payload_type) else {
        return Ok((candidate, TransformStats::default()));
    };
    let parent = required_batch(records, owner.parent_payload_type())?;
    let parent_ids = get_required_array(parent, consts::ID)?.clone();
    preflight_attribute_transform(records, attrs, transform, limits)?;
    let (transformed, stats) = transform_attributes_with_stats(attrs, &parent_ids, transform)?;

    if transformed.num_rows() == 0 {
        candidate.remove(payload_type);
    } else {
        candidate.set(payload_type, normalize_attribute_ordinals(transformed)?)?;
    }

    Ok((finalize(candidate, limits)?, stats))
}

/// Transform shared resource or scope attributes for a Profiles BAR.
///
/// These operations are BAR-global for the referenced owner IDs. Value-changing
/// and value-creating actions remain rejected consistently with profile-owned
/// attributes.
pub fn transform_shared_profile_attributes(
    records: &OtapArrowRecords,
    payload_type: ArrowPayloadType,
    transform: &AttributesTransform,
    limits: ProfilesTransformLimits,
) -> Result<(OtapArrowRecords, TransformStats)> {
    if !matches!(
        payload_type,
        ArrowPayloadType::ResourceAttrs | ArrowPayloadType::ScopeAttrs
    ) {
        return Err(Error::UnsupportedProfilesTransform {
            operation: "shared Profiles attribute transform requires resource or scope attributes",
        });
    }
    validate_attribute_transform_support(transform)?;
    enforce_limits(records, limits)?;
    let mut candidate = clone_profiles(records)?;
    let Some(attrs) = records.get(payload_type) else {
        return Ok((candidate, TransformStats::default()));
    };
    preflight_attribute_transform(records, attrs, transform, limits)?;
    let stats = super::apply_attribute_transform(&mut candidate, payload_type, transform, true)?
        .unwrap_or_default();
    Ok((finalize(candidate, limits)?, stats))
}

/// Revalidate and enforce limits for a complete Profiles transform candidate.
pub fn validate_profile_transform_output(
    candidate: OtapArrowRecords,
    limits: ProfilesTransformLimits,
) -> Result<OtapArrowRecords> {
    finalize(candidate, limits)
}

/// Replace every present function filename in the BAR.
///
/// This operation is global to each function owner and therefore affects every
/// stack path that references that function.
pub fn redact_profile_function_filenames_global(
    records: &OtapArrowRecords,
    replacement: &str,
    limits: ProfilesTransformLimits,
) -> Result<(OtapArrowRecords, ProfilesRedactionStats)> {
    enforce_limits(records, limits)?;
    let mut candidate = clone_profiles(records)?;
    let Some(functions) = records.get(ArrowPayloadType::ProfileFunctions) else {
        return Ok((candidate, ProfilesRedactionStats::default()));
    };
    validate_function_redaction(functions, None, replacement)?;
    let redacted = count_present_strings(functions, consts::FILENAME)?;
    if redacted == 0 {
        return Ok((candidate, ProfilesRedactionStats::default()));
    }
    preflight_replacement_bytes(records, redacted, replacement.len(), limits)?;
    let functions =
        replace_existing_string_values(functions.clone(), consts::FILENAME, replacement)?;
    candidate.set(ArrowPayloadType::ProfileFunctions, functions)?;
    Ok((
        finalize(candidate, limits)?,
        ProfilesRedactionStats {
            functions_redacted: u64::try_from(redacted).unwrap_or(u64::MAX),
            cloned_rows: 0,
        },
    ))
}

/// Redact function filenames only on paths reached by selected samples.
///
/// The operation clones every selected function, complete location, line row,
/// location attribute row, stack, and stack-location edge before repointing
/// selected samples. Non-selected samples retain the original graph.
pub fn redact_profile_function_filenames_for_samples(
    records: &OtapArrowRecords,
    selected_sample_ids: &HashSet<u32>,
    replacement: &str,
    limits: ProfilesTransformLimits,
) -> Result<(OtapArrowRecords, ProfilesRedactionStats)> {
    enforce_limits(records, limits)?;
    if selected_sample_ids.is_empty() {
        return Ok((clone_profiles(records)?, ProfilesRedactionStats::default()));
    }
    if selected_sample_ids.len() > limits.max_cloned_rows {
        return Err(Error::ProfilesTransformLimitExceeded {
            limit: "selected samples",
            actual: selected_sample_ids.len(),
            maximum: limits.max_cloned_rows,
        });
    }

    let samples = required_batch(records, ArrowPayloadType::Samples)?;
    let sample_ids = get_u32_array(samples, consts::ID)?;
    let mut found_sample_ids = HashSet::with_capacity(selected_sample_ids.len());
    for id in sample_ids.values() {
        if selected_sample_ids.contains(id) {
            let _ = found_sample_ids.insert(*id);
        }
    }
    if let Some(missing) = selected_sample_ids
        .iter()
        .find(|id| !found_sample_ids.contains(id))
    {
        return Err(Error::Format {
            error: format!("selected sample ID {missing} does not exist"),
        });
    }

    let stack_refs = samples
        .column_by_name(consts::STACK_ID)
        .map(UInt32ArrayAccessor::try_new)
        .transpose()?;
    let mut selected_stack_ids = HashSet::new();
    let mut cloned_rows = 0usize;
    for row in 0..samples.num_rows() {
        if selected_sample_ids.contains(&sample_ids.value(row))
            && let Some(stack_id) = stack_refs.as_ref().and_then(|refs| refs.value_at(row))
            && selected_stack_ids.insert(stack_id)
        {
            add_clone_rows(&mut cloned_rows, 1, limits)?;
        }
    }
    if selected_stack_ids.is_empty() {
        return Ok((clone_profiles(records)?, ProfilesRedactionStats::default()));
    }

    let stack_locations = required_batch(records, ArrowPayloadType::StackLocations)?;
    let stack_edge_rows = matching_row_indices_bounded(
        stack_locations,
        consts::PARENT_ID,
        &selected_stack_ids,
        limits.max_cloned_rows.saturating_sub(cloned_rows),
    )?;
    add_clone_rows(&mut cloned_rows, stack_edge_rows.len(), limits)?;
    let selected_location_ids =
        referenced_ids_at_rows(stack_locations, consts::LOCATION_ID, &stack_edge_rows)?;
    add_clone_rows(&mut cloned_rows, selected_location_ids.len(), limits)?;
    let location_lines = records.get(ArrowPayloadType::ProfileLocationLines);
    let line_rows = location_lines
        .map(|batch| {
            matching_row_indices_bounded(
                batch,
                consts::PARENT_ID,
                &selected_location_ids,
                limits.max_cloned_rows.saturating_sub(cloned_rows),
            )
        })
        .transpose()?
        .unwrap_or_default();
    add_clone_rows(&mut cloned_rows, line_rows.len(), limits)?;
    let selected_function_ids = location_lines
        .map(|batch| referenced_ids_at_rows(batch, consts::FUNCTION_ID, &line_rows))
        .transpose()?
        .unwrap_or_default();
    add_clone_rows(&mut cloned_rows, selected_function_ids.len(), limits)?;
    let selected_function_filenames = records
        .get(ArrowPayloadType::ProfileFunctions)
        .map(|batch| count_present_strings_for_ids(batch, &selected_function_ids))
        .transpose()?
        .unwrap_or_default();
    if let Some(functions) = records.get(ArrowPayloadType::ProfileFunctions) {
        validate_function_redaction(functions, Some(&selected_function_ids), replacement)?;
    }
    if selected_function_filenames == 0 {
        return Ok((clone_profiles(records)?, ProfilesRedactionStats::default()));
    }
    let location_attr_rows = records
        .get(ArrowPayloadType::ProfileLocationAttrs)
        .map(|batch| {
            matching_row_indices_bounded(
                batch,
                consts::PARENT_ID,
                &selected_location_ids,
                limits.max_cloned_rows.saturating_sub(cloned_rows),
            )
        })
        .transpose()?
        .unwrap_or_default();
    add_clone_rows(&mut cloned_rows, location_attr_rows.len(), limits)?;
    let output_rows = total_rows(records)?
        .checked_add(cloned_rows)
        .ok_or(Error::Dropped)?;
    if output_rows > limits.max_output_rows {
        return Err(Error::ProfilesTransformLimitExceeded {
            limit: "output rows",
            actual: output_rows,
            maximum: limits.max_output_rows,
        });
    }
    let stack_rows = matching_row_indices(
        required_batch(records, ArrowPayloadType::Stacks)?,
        consts::ID,
        &selected_stack_ids,
    )?;
    let location_rows = matching_row_indices(
        required_batch(records, ArrowPayloadType::ProfileLocations)?,
        consts::ID,
        &selected_location_ids,
    )?;
    let function_rows = records
        .get(ArrowPayloadType::ProfileFunctions)
        .map(|batch| matching_row_indices(batch, consts::ID, &selected_function_ids))
        .transpose()?
        .unwrap_or_default();
    preflight_clone_bytes(
        records,
        [
            (ArrowPayloadType::Stacks, stack_rows.as_slice()),
            (ArrowPayloadType::StackLocations, stack_edge_rows.as_slice()),
            (ArrowPayloadType::ProfileLocations, location_rows.as_slice()),
            (ArrowPayloadType::ProfileLocationLines, line_rows.as_slice()),
            (ArrowPayloadType::ProfileFunctions, function_rows.as_slice()),
            (
                ArrowPayloadType::ProfileLocationAttrs,
                location_attr_rows.as_slice(),
            ),
        ],
        selected_function_filenames,
        replacement.len(),
        cloned_rows,
        limits,
    )?;

    let stack_map = fresh_id_map(
        required_batch(records, ArrowPayloadType::Stacks)?,
        &selected_stack_ids,
    )?;
    let location_map = fresh_id_map(
        required_batch(records, ArrowPayloadType::ProfileLocations)?,
        &selected_location_ids,
    )?;
    let function_map = if selected_function_ids.is_empty() {
        HashMap::new()
    } else {
        fresh_id_map(
            required_batch(records, ArrowPayloadType::ProfileFunctions)?,
            &selected_function_ids,
        )?
    };

    let mut candidate = clone_profiles(records)?;
    clone_entity_rows(
        &mut candidate,
        ArrowPayloadType::ProfileFunctions,
        &function_map,
        Some(replacement),
    )?;
    clone_entity_rows(
        &mut candidate,
        ArrowPayloadType::ProfileLocations,
        &location_map,
        None,
    )?;
    clone_related_rows(
        &mut candidate,
        ArrowPayloadType::ProfileLocationLines,
        &location_map,
        Some((consts::FUNCTION_ID, &function_map)),
    )?;
    clone_related_rows(
        &mut candidate,
        ArrowPayloadType::ProfileLocationAttrs,
        &location_map,
        None,
    )?;
    clone_entity_rows(&mut candidate, ArrowPayloadType::Stacks, &stack_map, None)?;
    clone_related_rows(
        &mut candidate,
        ArrowPayloadType::StackLocations,
        &stack_map,
        Some((consts::LOCATION_ID, &location_map)),
    )?;
    repoint_selected_sample_stacks(&mut candidate, selected_sample_ids, &stack_map)?;

    Ok((
        finalize(candidate, limits)?,
        ProfilesRedactionStats {
            functions_redacted: u64::try_from(selected_function_filenames).unwrap_or(u64::MAX),
            cloned_rows: u64::try_from(cloned_rows).unwrap_or(u64::MAX),
        },
    ))
}

fn clone_profiles(records: &OtapArrowRecords) -> Result<OtapArrowRecords> {
    match records {
        OtapArrowRecords::Profiles(_) => Ok(records.clone()),
        OtapArrowRecords::Logs(_) => Err(Error::UnexpectedSignalType {
            found: SignalType::Logs,
            expected: SignalType::Profiles,
        }),
        OtapArrowRecords::Metrics(_) => Err(Error::UnexpectedSignalType {
            found: SignalType::Metrics,
            expected: SignalType::Profiles,
        }),
        OtapArrowRecords::Traces(_) => Err(Error::UnexpectedSignalType {
            found: SignalType::Traces,
            expected: SignalType::Profiles,
        }),
    }
}

fn required_batch(
    records: &OtapArrowRecords,
    payload_type: ArrowPayloadType,
) -> Result<&RecordBatch> {
    records
        .get(payload_type)
        .ok_or(Error::RecordBatchNotFound { payload_type })
}

fn finalize(
    candidate: OtapArrowRecords,
    limits: ProfilesTransformLimits,
) -> Result<OtapArrowRecords> {
    let OtapArrowRecords::Profiles(profiles) = candidate else {
        return Err(Error::UnexpectedSignalType {
            found: SignalType::Logs,
            expected: SignalType::Profiles,
        });
    };
    let output = OtapArrowRecords::Profiles(Profiles::try_from(profiles.into_raw())?);
    enforce_limits(&output, limits)?;
    Ok(output)
}

fn enforce_limits(records: &OtapArrowRecords, limits: ProfilesTransformLimits) -> Result<()> {
    let rows = total_rows(records)?;
    if rows > limits.max_output_rows {
        return Err(Error::ProfilesTransformLimitExceeded {
            limit: "output rows",
            actual: rows,
            maximum: limits.max_output_rows,
        });
    }
    let bytes = records.retained_memory_bytes();
    if bytes > limits.max_output_bytes {
        return Err(Error::ProfilesTransformLimitExceeded {
            limit: "output bytes",
            actual: bytes,
            maximum: limits.max_output_bytes,
        });
    }
    Ok(())
}

fn total_rows(records: &OtapArrowRecords) -> Result<usize> {
    records
        .allowed_payload_types()
        .iter()
        .filter_map(|payload_type| records.get(*payload_type))
        .try_fold(0usize, |total, batch| {
            total
                .checked_add(batch.num_rows())
                .ok_or(Error::ProfilesTransformLimitExceeded {
                    limit: "output rows",
                    actual: usize::MAX,
                    maximum: usize::MAX,
                })
        })
}

fn entity_ids(batch: &RecordBatch, column: &'static str) -> Result<HashSet<u32>> {
    let ids = get_u32_array(batch, column)?;
    Ok(ids.values().iter().copied().collect())
}

fn referenced_ids(batch: &RecordBatch, column: &'static str) -> Result<HashSet<u32>> {
    let Some(array) = batch.column_by_name(column) else {
        return Ok(HashSet::new());
    };
    let ids = UInt32ArrayAccessor::try_new(array)?;
    Ok((0..ids.len()).filter_map(|row| ids.value_at(row)).collect())
}

fn filter_batch_by_ids(
    batch: &RecordBatch,
    column: &'static str,
    retained_ids: &HashSet<u32>,
) -> Result<RecordBatch> {
    let values = UInt32ArrayAccessor::try_new_for_column(batch, column)?;
    let mut selection = BooleanBuilder::with_capacity(batch.num_rows());
    for row in 0..batch.num_rows() {
        selection.append_value(
            values
                .value_at(row)
                .is_some_and(|id| retained_ids.contains(&id)),
        );
    }
    filter_record_batch(batch, &selection.finish())
        .map_err(|source| Error::ColumnLengthMismatch { source })
}

fn retain_optional_rows(
    records: &mut OtapArrowRecords,
    payload_type: ArrowPayloadType,
    column: &'static str,
    retained_ids: &HashSet<u32>,
) -> Result<()> {
    let Some(batch) = records.get(payload_type).cloned() else {
        return Ok(());
    };
    let filtered = filter_batch_by_ids(&batch, column, retained_ids)?;
    if filtered.num_rows() == 0 {
        records.remove(payload_type);
    } else {
        records.set(payload_type, filtered)?;
    }
    Ok(())
}

fn normalize_attribute_ordinals(batch: RecordBatch) -> Result<RecordBatch> {
    if batch.num_rows() == 0 {
        return Ok(batch);
    }
    let parents = UInt32ArrayAccessor::try_new_for_column(&batch, consts::PARENT_ID)?;
    let ordinals = get_u32_array(&batch, consts::ORDINAL)?;
    let mut indices = (0..batch.num_rows())
        .map(|row| {
            u32::try_from(row).map_err(|_| Error::ProfilesTransformLimitExceeded {
                limit: "attribute rows",
                actual: batch.num_rows(),
                maximum: u32::MAX as usize,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    indices.sort_unstable_by_key(|row| {
        let row = *row as usize;
        (
            parents.value_at(row).unwrap_or_default(),
            ordinals.value(row),
        )
    });

    let indices = UInt32Array::from(indices);
    let sorted_columns = batch
        .columns()
        .iter()
        .map(|column| take(column, &indices, None))
        .collect::<arrow::error::Result<Vec<_>>>()
        .map_err(|source| Error::ColumnLengthMismatch { source })?;
    let sorted = RecordBatch::try_new(batch.schema(), sorted_columns)
        .map_err(|source| Error::ColumnLengthMismatch { source })?;
    let sorted_parents = UInt32ArrayAccessor::try_new_for_column(&sorted, consts::PARENT_ID)?;
    let mut next_by_parent = HashMap::<u32, u32>::new();
    let mut new_ordinals = Vec::with_capacity(sorted.num_rows());
    for row in 0..sorted.num_rows() {
        let parent = sorted_parents.value_at(row).unwrap_or_default();
        let next = next_by_parent.entry(parent).or_default();
        new_ordinals.push(Some(*next));
        *next = next
            .checked_add(1)
            .ok_or(Error::ProfilesTransformLimitExceeded {
                limit: "attribute ordinals",
                actual: sorted.num_rows(),
                maximum: u32::MAX as usize,
            })?;
    }
    replace_u32_column(sorted, consts::ORDINAL, new_ordinals)
}

fn validate_attribute_transform_support(transform: &AttributesTransform) -> Result<()> {
    if transform.may_create_new_attributes()
        || transform
            .update
            .as_ref()
            .is_some_and(|update| !update.entries.is_empty())
        || transform
            .hash
            .as_ref()
            .is_some_and(|hash| !hash.entries.is_empty())
    {
        return Err(Error::UnsupportedProfilesTransform {
            operation: "Profiles insert, upsert, update, and hash require an extended-schema value builder",
        });
    }
    Ok(())
}

fn preflight_attribute_transform(
    records: &OtapArrowRecords,
    attrs: &RecordBatch,
    transform: &AttributesTransform,
    limits: ProfilesTransformLimits,
) -> Result<()> {
    let mut per_row_growth = 0usize;
    if let Some(rename) = &transform.rename {
        per_row_growth = per_row_growth
            .checked_add(
                rename
                    .map
                    .values()
                    .map(String::len)
                    .max()
                    .unwrap_or_default(),
            )
            .ok_or(Error::Dropped)?;
    }
    let maximum = records
        .retained_memory_bytes()
        .checked_add(
            attrs
                .num_rows()
                .checked_mul(per_row_growth)
                .ok_or(Error::Dropped)?,
        )
        .ok_or(Error::Dropped)?;
    if maximum > limits.max_output_bytes {
        return Err(Error::ProfilesTransformLimitExceeded {
            limit: "output bytes",
            actual: maximum,
            maximum: limits.max_output_bytes,
        });
    }
    Ok(())
}

fn count_present_strings(batch: &RecordBatch, column: &'static str) -> Result<usize> {
    let Some(array) = batch.column_by_name(column) else {
        return Ok(0);
    };
    let values = StringArrayAccessor::try_new(array)?;
    Ok((0..values.len())
        .filter(|row| values.str_at(*row).is_some_and(|value| !value.is_empty()))
        .count())
}

fn count_present_strings_for_ids(
    batch: &RecordBatch,
    selected_ids: &HashSet<u32>,
) -> Result<usize> {
    let ids = get_u32_array(batch, consts::ID)?;
    let Some(array) = batch.column_by_name(consts::FILENAME) else {
        return Ok(0);
    };
    let values = StringArrayAccessor::try_new(array)?;
    Ok((0..batch.num_rows())
        .filter(|row| {
            selected_ids.contains(&ids.value(*row))
                && values.str_at(*row).is_some_and(|value| !value.is_empty())
        })
        .count())
}

fn validate_function_redaction(
    functions: &RecordBatch,
    selected_ids: Option<&HashSet<u32>>,
    replacement: &str,
) -> Result<()> {
    if !replacement.is_empty() {
        return Ok(());
    }
    let ids = get_u32_array(functions, consts::ID)?;
    let names = functions
        .column_by_name(consts::NAME)
        .map(StringArrayAccessor::try_new)
        .transpose()?;
    let system_names = functions
        .column_by_name(consts::SYSTEM_NAME)
        .map(StringArrayAccessor::try_new)
        .transpose()?;
    let filenames = functions
        .column_by_name(consts::FILENAME)
        .map(StringArrayAccessor::try_new)
        .transpose()?;
    for row in 0..functions.num_rows() {
        let id = ids.value(row);
        if selected_ids.is_some_and(|selected| !selected.contains(&id)) {
            continue;
        }
        let filename_present = filenames
            .as_ref()
            .and_then(|values| values.str_at(row))
            .is_some_and(|value| !value.is_empty());
        let name_empty = names
            .as_ref()
            .and_then(|values| values.str_at(row))
            .unwrap_or_default()
            .is_empty();
        let system_name_empty = system_names
            .as_ref()
            .and_then(|values| values.str_at(row))
            .unwrap_or_default()
            .is_empty();
        if filename_present && name_empty && system_name_empty {
            return Err(Error::UnsupportedProfilesTransform {
                operation: "empty filename redaction would create a zero function",
            });
        }
    }
    Ok(())
}

fn preflight_replacement_bytes(
    records: &OtapArrowRecords,
    values: usize,
    replacement_len: usize,
    limits: ProfilesTransformLimits,
) -> Result<()> {
    let estimated = records
        .retained_memory_bytes()
        .checked_add(values.checked_mul(replacement_len).ok_or(Error::Dropped)?)
        .ok_or(Error::Dropped)?;
    if estimated > limits.max_output_bytes {
        return Err(Error::ProfilesTransformLimitExceeded {
            limit: "output bytes",
            actual: estimated,
            maximum: limits.max_output_bytes,
        });
    }
    Ok(())
}

fn matching_row_indices(
    batch: &RecordBatch,
    column: &'static str,
    ids: &HashSet<u32>,
) -> Result<Vec<u32>> {
    let values = UInt32ArrayAccessor::try_new_for_column(batch, column)?;
    (0..batch.num_rows())
        .filter(|row| values.value_at(*row).is_some_and(|id| ids.contains(&id)))
        .map(|row| {
            u32::try_from(row).map_err(|_| Error::ProfilesTransformLimitExceeded {
                limit: "clone row indices",
                actual: batch.num_rows(),
                maximum: u32::MAX as usize,
            })
        })
        .collect()
}

fn matching_row_indices_bounded(
    batch: &RecordBatch,
    column: &'static str,
    ids: &HashSet<u32>,
    maximum: usize,
) -> Result<Vec<u32>> {
    let values = UInt32ArrayAccessor::try_new_for_column(batch, column)?;
    let mut rows = Vec::new();
    for row in 0..batch.num_rows() {
        if values.value_at(row).is_some_and(|id| ids.contains(&id)) {
            if rows.len() == maximum {
                return Err(Error::ProfilesTransformLimitExceeded {
                    limit: "cloned rows",
                    actual: rows.len().saturating_add(1),
                    maximum,
                });
            }
            rows.push(
                u32::try_from(row).map_err(|_| Error::ProfilesTransformLimitExceeded {
                    limit: "clone row indices",
                    actual: batch.num_rows(),
                    maximum: u32::MAX as usize,
                })?,
            );
        }
    }
    Ok(rows)
}

fn add_clone_rows(
    current: &mut usize,
    additional: usize,
    limits: ProfilesTransformLimits,
) -> Result<()> {
    *current = current.checked_add(additional).ok_or(Error::Dropped)?;
    if *current > limits.max_cloned_rows {
        return Err(Error::ProfilesTransformLimitExceeded {
            limit: "cloned rows",
            actual: *current,
            maximum: limits.max_cloned_rows,
        });
    }
    Ok(())
}

fn referenced_ids_at_rows(
    batch: &RecordBatch,
    column: &'static str,
    rows: &[u32],
) -> Result<HashSet<u32>> {
    let Some(array) = batch.column_by_name(column) else {
        return Ok(HashSet::new());
    };
    let values = UInt32ArrayAccessor::try_new(array)?;
    Ok(rows
        .iter()
        .filter_map(|row| values.value_at(*row as usize))
        .collect())
}

fn preflight_clone_bytes<const N: usize>(
    records: &OtapArrowRecords,
    clone_rows: [(ArrowPayloadType, &[u32]); N],
    replacement_values: usize,
    replacement_len: usize,
    cloned_rows: usize,
    limits: ProfilesTransformLimits,
) -> Result<()> {
    let mut estimated = records.retained_memory_bytes();
    let materializes_function_filenames = clone_rows.iter().any(|(payload_type, rows)| {
        *payload_type == ArrowPayloadType::ProfileFunctions && !rows.is_empty()
    });
    let mut seen = crate::otap::memory::CountedAllocations::default();
    for (payload_type, rows) in clone_rows {
        if rows.is_empty() {
            continue;
        }
        let batch = required_batch(records, payload_type)?;
        estimated = estimated
            .checked_add(crate::otap::memory::record_batch_pinned_bytes(
                batch, &mut seen,
            ))
            .ok_or(Error::Dropped)?;
    }
    if materializes_function_filenames
        && let Some(functions) = records.get(ArrowPayloadType::ProfileFunctions)
    {
        estimated = estimated
            .checked_add(expanded_string_column_bytes(functions, consts::FILENAME)?)
            .ok_or(Error::Dropped)?;
    }
    estimated = estimated
        .checked_add(
            replacement_values
                .checked_mul(replacement_len)
                .ok_or(Error::Dropped)?,
        )
        .ok_or(Error::Dropped)?;
    estimated = estimated
        .checked_add(cloned_rows.checked_mul(64).ok_or(Error::Dropped)?)
        .ok_or(Error::Dropped)?;
    if estimated > limits.max_output_bytes {
        return Err(Error::ProfilesTransformLimitExceeded {
            limit: "output bytes",
            actual: estimated,
            maximum: limits.max_output_bytes,
        });
    }
    Ok(())
}

fn expanded_string_column_bytes(batch: &RecordBatch, column: &'static str) -> Result<usize> {
    let Some(array) = batch.column_by_name(column) else {
        return Ok(0);
    };
    let values = StringArrayAccessor::try_new(array)?;
    (0..values.len()).try_fold(
        (values.len() + 1)
            .checked_mul(size_of::<i32>())
            .ok_or(Error::Dropped)?,
        |total, row| {
            total
                .checked_add(values.str_at(row).map_or(0, str::len))
                .ok_or(Error::Dropped)
        },
    )
}

fn fresh_id_map(batch: &RecordBatch, selected_ids: &HashSet<u32>) -> Result<HashMap<u32, u32>> {
    let ids = get_u32_array(batch, consts::ID)?;
    let mut found = HashSet::with_capacity(selected_ids.len());
    let mut next = 0;
    for id in ids.values() {
        next = next.max(*id);
        if selected_ids.contains(id) {
            let _ = found.insert(*id);
        }
    }
    if let Some(missing) = selected_ids.iter().find(|id| !found.contains(id)) {
        return Err(Error::Format {
            error: format!("cannot clone missing entity ID {missing}"),
        });
    }
    let mut selected: Vec<_> = selected_ids.iter().copied().collect();
    selected.sort_unstable();
    selected
        .into_iter()
        .map(|id| {
            next = next
                .checked_add(1)
                .ok_or(Error::ProfilesTransformLimitExceeded {
                    limit: "fresh entity IDs",
                    actual: usize::MAX,
                    maximum: u32::MAX as usize,
                })?;
            Ok((id, next))
        })
        .collect()
}

fn clone_entity_rows(
    records: &mut OtapArrowRecords,
    payload_type: ArrowPayloadType,
    id_map: &HashMap<u32, u32>,
    filename_replacement: Option<&str>,
) -> Result<()> {
    if id_map.is_empty() {
        return Ok(());
    }
    let mut batch = required_batch(records, payload_type)?.clone();
    if filename_replacement.is_some() {
        batch = materialize_string_column(batch, consts::FILENAME)?;
    }
    let selected: HashSet<_> = id_map.keys().copied().collect();
    let rows = matching_row_indices(&batch, consts::ID, &selected)?;
    let mut cloned = take_rows(&batch, &rows)?;
    cloned = remap_u32_values(cloned, consts::ID, id_map)?;
    if let Some(replacement) = filename_replacement {
        cloned = replace_existing_string_values(cloned, consts::FILENAME, replacement)?;
    }
    let combined = concat_batches(batch.schema_ref(), [&batch, &cloned])
        .map_err(|source| Error::ColumnLengthMismatch { source })?;
    records.set(payload_type, combined)
}

fn clone_related_rows(
    records: &mut OtapArrowRecords,
    payload_type: ArrowPayloadType,
    parent_map: &HashMap<u32, u32>,
    reference_map: Option<(&'static str, &HashMap<u32, u32>)>,
) -> Result<()> {
    if parent_map.is_empty() || records.get(payload_type).is_none() {
        return Ok(());
    }
    let batch = materialize_u32_column(
        required_batch(records, payload_type)?.clone(),
        consts::PARENT_ID,
    )?;
    let selected: HashSet<_> = parent_map.keys().copied().collect();
    let rows = matching_row_indices(&batch, consts::PARENT_ID, &selected)?;
    if rows.is_empty() {
        return Ok(());
    }
    let mut cloned = take_rows(&batch, &rows)?;
    cloned = remap_u32_values(cloned, consts::PARENT_ID, parent_map)?;
    if let Some((column, mapping)) = reference_map {
        cloned = remap_u32_values(cloned, column, mapping)?;
    }
    let combined = concat_batches(batch.schema_ref(), [&batch, &cloned])
        .map_err(|source| Error::ColumnLengthMismatch { source })?;
    records.set(payload_type, combined)
}

fn repoint_selected_sample_stacks(
    records: &mut OtapArrowRecords,
    selected_sample_ids: &HashSet<u32>,
    stack_map: &HashMap<u32, u32>,
) -> Result<()> {
    let samples = required_batch(records, ArrowPayloadType::Samples)?.clone();
    let ids = get_u32_array(&samples, consts::ID)?;
    let stack_refs = samples
        .column_by_name(consts::STACK_ID)
        .map(UInt32ArrayAccessor::try_new)
        .transpose()?;
    let Some(stack_refs) = stack_refs else {
        return Ok(());
    };
    let mut output = Vec::with_capacity(samples.num_rows());
    for row in 0..samples.num_rows() {
        let stack_id = stack_refs.value_at(row);
        if selected_sample_ids.contains(&ids.value(row)) {
            output.push(
                stack_id
                    .map(|id| {
                        stack_map.get(&id).copied().ok_or_else(|| Error::Format {
                            error: format!("missing cloned stack for selected sample stack {id}"),
                        })
                    })
                    .transpose()?,
            );
        } else {
            output.push(stack_id);
        }
    }
    records.set(
        ArrowPayloadType::Samples,
        replace_u32_column(samples, consts::STACK_ID, output)?,
    )
}

fn take_rows(batch: &RecordBatch, rows: &[u32]) -> Result<RecordBatch> {
    let indices = UInt32Array::from(rows.to_vec());
    let columns = batch
        .columns()
        .iter()
        .map(|column| take(column, &indices, None))
        .collect::<arrow::error::Result<Vec<_>>>()
        .map_err(|source| Error::ColumnLengthMismatch { source })?;
    RecordBatch::try_new(batch.schema(), columns)
        .map_err(|source| Error::ColumnLengthMismatch { source })
}

fn materialize_u32_column(batch: RecordBatch, column: &'static str) -> Result<RecordBatch> {
    let Some(array) = batch.column_by_name(column) else {
        return Ok(batch);
    };
    let values = UInt32ArrayAccessor::try_new(array)?;
    let output = (0..values.len()).map(|row| values.value_at(row)).collect();
    replace_u32_column(batch, column, output)
}

fn materialize_string_column(batch: RecordBatch, column: &'static str) -> Result<RecordBatch> {
    let Some(array) = batch.column_by_name(column) else {
        return Ok(batch);
    };
    let values = StringArrayAccessor::try_new(array)?;
    let output = (0..values.len())
        .map(|row| values.str_at(row).map(str::to_string))
        .collect::<Vec<_>>();
    replace_array_column(batch, column, Arc::new(StringArray::from(output)))
}

fn replace_existing_string_values(
    batch: RecordBatch,
    column: &'static str,
    replacement: &str,
) -> Result<RecordBatch> {
    let Some(array) = batch.column_by_name(column) else {
        return Ok(batch);
    };
    let values = StringArrayAccessor::try_new(array)?;
    let output = (0..values.len())
        .map(|row| {
            values.str_at(row).map(|value| {
                if value.is_empty() {
                    String::new()
                } else {
                    replacement.to_string()
                }
            })
        })
        .collect::<Vec<_>>();
    replace_array_column(batch, column, Arc::new(StringArray::from(output)))
}

fn remap_u32_values(
    batch: RecordBatch,
    column: &'static str,
    mapping: &HashMap<u32, u32>,
) -> Result<RecordBatch> {
    let Some(array) = batch.column_by_name(column) else {
        return Ok(batch);
    };
    let values = UInt32ArrayAccessor::try_new(array)?;
    let output = (0..values.len())
        .map(|row| {
            values
                .value_at(row)
                .map(|id| {
                    mapping.get(&id).copied().ok_or_else(|| Error::Format {
                        error: format!("missing cloned ID mapping for {column} value {id}"),
                    })
                })
                .transpose()
        })
        .collect::<Result<Vec<_>>>()?;
    replace_u32_column(batch, column, output)
}

fn replace_array_column(
    batch: RecordBatch,
    column: &'static str,
    array: ArrayRef,
) -> Result<RecordBatch> {
    let schema = batch.schema();
    let index = schema.index_of(column).map_err(|_| Error::ColumnNotFound {
        name: column.to_string(),
    })?;
    let mut fields = schema.fields().to_vec();
    let mut field_metadata = fields[index].metadata().clone();
    let _ = field_metadata.insert(
        metadata::COLUMN_ENCODING.to_string(),
        metadata::encodings::PLAIN.to_string(),
    );
    fields[index] = Arc::new(
        fields[index]
            .as_ref()
            .clone()
            .with_data_type(array.data_type().clone())
            .with_metadata(field_metadata),
    );
    let mut columns = batch.columns().to_vec();
    columns[index] = array;
    RecordBatch::try_new(
        Arc::new(Schema::new(fields).with_metadata(schema.metadata().clone())),
        columns,
    )
    .map_err(|source| Error::ColumnLengthMismatch { source })
}

fn remap_dense_ids(records: &mut OtapArrowRecords) -> Result<u64> {
    let profile_ids = dense_id_map(records, ArrowPayloadType::Profiles)?;
    let sample_ids = dense_id_map(records, ArrowPayloadType::Samples)?;
    let stack_ids = dense_id_map(records, ArrowPayloadType::Stacks)?;
    let location_ids = dense_id_map(records, ArrowPayloadType::ProfileLocations)?;
    let function_ids = dense_id_map(records, ArrowPayloadType::ProfileFunctions)?;
    let mapping_ids = dense_id_map(records, ArrowPayloadType::ProfileMappings)?;
    let link_ids = dense_id_map(records, ArrowPayloadType::ProfileLinks)?;

    let mut rewritten = 0u64;
    rewritten += remap_payload_column(
        records,
        ArrowPayloadType::Profiles,
        consts::ID,
        &profile_ids,
    )?;
    rewritten += remap_payload_column(records, ArrowPayloadType::Samples, consts::ID, &sample_ids)?;
    rewritten += remap_payload_column(
        records,
        ArrowPayloadType::Samples,
        consts::PARENT_ID,
        &profile_ids,
    )?;
    rewritten += remap_payload_column(
        records,
        ArrowPayloadType::Samples,
        consts::STACK_ID,
        &stack_ids,
    )?;
    rewritten += remap_payload_column(
        records,
        ArrowPayloadType::Samples,
        consts::LINK_ID,
        &link_ids,
    )?;
    rewritten += remap_payload_column(
        records,
        ArrowPayloadType::ProfileValueTypes,
        consts::PARENT_ID,
        &profile_ids,
    )?;
    rewritten += remap_payload_column(
        records,
        ArrowPayloadType::ProfileAttrs,
        consts::PARENT_ID,
        &profile_ids,
    )?;
    rewritten += remap_payload_column(
        records,
        ArrowPayloadType::ProfileSampleAttrs,
        consts::PARENT_ID,
        &sample_ids,
    )?;
    rewritten += remap_payload_column(records, ArrowPayloadType::Stacks, consts::ID, &stack_ids)?;
    rewritten += remap_payload_column(
        records,
        ArrowPayloadType::StackLocations,
        consts::PARENT_ID,
        &stack_ids,
    )?;
    rewritten += remap_payload_column(
        records,
        ArrowPayloadType::StackLocations,
        consts::LOCATION_ID,
        &location_ids,
    )?;
    rewritten += remap_payload_column(
        records,
        ArrowPayloadType::ProfileLocations,
        consts::ID,
        &location_ids,
    )?;
    rewritten += remap_payload_column(
        records,
        ArrowPayloadType::ProfileLocations,
        consts::MAPPING_ID,
        &mapping_ids,
    )?;
    rewritten += remap_payload_column(
        records,
        ArrowPayloadType::ProfileLocationLines,
        consts::PARENT_ID,
        &location_ids,
    )?;
    rewritten += remap_payload_column(
        records,
        ArrowPayloadType::ProfileLocationLines,
        consts::FUNCTION_ID,
        &function_ids,
    )?;
    rewritten += remap_payload_column(
        records,
        ArrowPayloadType::ProfileFunctions,
        consts::ID,
        &function_ids,
    )?;
    rewritten += remap_payload_column(
        records,
        ArrowPayloadType::ProfileMappings,
        consts::ID,
        &mapping_ids,
    )?;
    rewritten += remap_payload_column(
        records,
        ArrowPayloadType::ProfileLinks,
        consts::ID,
        &link_ids,
    )?;
    rewritten += remap_payload_column(
        records,
        ArrowPayloadType::ProfileMappingAttrs,
        consts::PARENT_ID,
        &mapping_ids,
    )?;
    rewritten += remap_payload_column(
        records,
        ArrowPayloadType::ProfileLocationAttrs,
        consts::PARENT_ID,
        &location_ids,
    )?;
    Ok(rewritten)
}

fn dense_id_map(
    records: &OtapArrowRecords,
    payload_type: ArrowPayloadType,
) -> Result<HashMap<u32, u32>> {
    let Some(batch) = records.get(payload_type) else {
        return Ok(HashMap::new());
    };
    let mut ids = get_u32_array(batch, consts::ID)?.values().to_vec();
    ids.sort_unstable();
    ids.into_iter()
        .enumerate()
        .map(|(index, id)| {
            let new_id =
                u32::try_from(index + 1).map_err(|_| Error::ProfilesTransformLimitExceeded {
                    limit: "dense IDs",
                    actual: index + 1,
                    maximum: u32::MAX as usize,
                })?;
            Ok((id, new_id))
        })
        .collect()
}

fn remap_payload_column(
    records: &mut OtapArrowRecords,
    payload_type: ArrowPayloadType,
    column: &'static str,
    mapping: &HashMap<u32, u32>,
) -> Result<u64> {
    let Some(batch) = records.get(payload_type).cloned() else {
        return Ok(0);
    };
    let Some(values) = batch.column_by_name(column) else {
        return Ok(0);
    };
    let values = UInt32ArrayAccessor::try_new(values)?;
    let mut rewritten = 0u64;
    let mut output = Vec::with_capacity(values.len());
    for row in 0..values.len() {
        let value = values.value_at(row);
        let mapped = value
            .map(|value| {
                mapping.get(&value).copied().ok_or_else(|| Error::Format {
                    error: format!(
                        "missing dense ID mapping for {column} value {value} in {payload_type:?}"
                    ),
                })
            })
            .transpose()?;
        if value != mapped {
            rewritten = rewritten.saturating_add(1);
        }
        output.push(mapped);
    }
    records.set(payload_type, replace_u32_column(batch, column, output)?)?;
    Ok(rewritten)
}

fn replace_u32_column(
    batch: RecordBatch,
    column: &'static str,
    values: Vec<Option<u32>>,
) -> Result<RecordBatch> {
    let schema = batch.schema();
    let index = schema.index_of(column).map_err(|_| Error::ColumnNotFound {
        name: column.to_string(),
    })?;
    let mut fields = schema.fields().to_vec();
    let mut field_metadata = fields[index].metadata().clone();
    let _ = field_metadata.insert(
        metadata::COLUMN_ENCODING.to_string(),
        metadata::encodings::PLAIN.to_string(),
    );
    fields[index] = Arc::new(
        fields[index]
            .as_ref()
            .clone()
            .with_data_type(DataType::UInt32)
            .with_metadata(field_metadata),
    );
    let mut columns = batch.columns().to_vec();
    columns[index] = Arc::new(UInt32Array::from(values)) as ArrayRef;
    RecordBatch::try_new(
        Arc::new(Schema::new(fields).with_metadata(schema.metadata().clone())),
        columns,
    )
    .map_err(|source| Error::ColumnLengthMismatch { source })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use arrow::array::UInt16Array;

    use super::*;
    use crate::arrays::StringArrayAccessor;
    use crate::encode::encode_profiles_otap_batch;
    use crate::otap::OtapBatchStore;
    use crate::otap::transform::{DeleteTransform, RenameTransform};
    use crate::testing::profiles::{ProfilesDatasetKind, profiles_dataset};

    fn cpu_records(samples: usize) -> OtapArrowRecords {
        encode_profiles_otap_batch(&profiles_dataset(ProfilesDatasetKind::Cpu, 1, samples, 2))
            .unwrap()
    }

    fn with_column_encoding(
        batch: RecordBatch,
        column: &'static str,
        encoding: &'static str,
    ) -> RecordBatch {
        let schema = crate::schema::update_field_metadata(
            batch.schema_ref(),
            column,
            metadata::COLUMN_ENCODING,
            encoding,
        );
        RecordBatch::try_new(Arc::new(schema), batch.columns().to_vec()).unwrap()
    }

    /// Scenario: A sample-aligned filter keeps alternating samples from one valid graph.
    /// Guarantees: Sample attributes follow their owners while shared dimensions remain intact.
    #[test]
    fn filters_samples_without_implicit_compaction() {
        let records = cpu_records(4);
        let stack_rows = records.get(ArrowPayloadType::Stacks).unwrap().num_rows();
        let selection = BooleanArray::from(vec![true, false, true, false]);

        let (filtered, stats) =
            filter_profile_samples(&records, &selection, ProfilesTransformLimits::default())
                .unwrap();

        assert_eq!(stats.samples_before, 4);
        assert_eq!(stats.samples_after, 2);
        assert_eq!(stats.samples_dropped(), 2);
        assert_eq!(
            filtered.get(ArrowPayloadType::Samples).unwrap().num_rows(),
            2
        );
        assert_eq!(
            filtered
                .get(ArrowPayloadType::ProfileSampleAttrs)
                .unwrap()
                .num_rows(),
            2
        );
        assert_eq!(
            filtered.get(ArrowPayloadType::Stacks).unwrap().num_rows(),
            stack_rows
        );
    }

    /// Scenario: Every sample is removed and graph compaction runs explicitly afterward.
    /// Guarantees: Root profiles remain while all now-unreachable shared dimensions are removed.
    #[test]
    fn compacts_dimensions_after_all_samples_are_filtered() {
        let records = cpu_records(2);
        let selection = BooleanArray::from(vec![false, false]);
        let (filtered, _) =
            filter_profile_samples(&records, &selection, ProfilesTransformLimits::default())
                .unwrap();
        assert!(filtered.get(ArrowPayloadType::Stacks).is_some());

        let (compacted, stats) = compact_profile_dimensions(
            &filtered,
            ProfilesCompactionOptions::default(),
            ProfilesTransformLimits::default(),
        )
        .unwrap();

        assert!(stats.rows_after < stats.rows_before);
        assert_eq!(
            compacted
                .get(ArrowPayloadType::Profiles)
                .unwrap()
                .num_rows(),
            1
        );
        assert_eq!(
            compacted.get(ArrowPayloadType::Samples).unwrap().num_rows(),
            0
        );
        for payload_type in [
            ArrowPayloadType::Stacks,
            ArrowPayloadType::StackLocations,
            ArrowPayloadType::ProfileLocations,
            ArrowPayloadType::ProfileLocationLines,
            ArrowPayloadType::ProfileFunctions,
            ArrowPayloadType::ProfileMappings,
            ArrowPayloadType::ProfileLinks,
            ArrowPayloadType::ProfileMappingAttrs,
            ArrowPayloadType::ProfileLocationAttrs,
        ] {
            assert!(
                compacted.get(payload_type).is_none(),
                "{payload_type:?} should be removed"
            );
        }
    }

    /// Scenario: Filtering leaves only a sample that references the second stack dictionary row.
    /// Guarantees: Dense compaction rewrites both the stack ID and sample foreign key to one.
    #[test]
    fn dense_compaction_rewrites_entity_references() {
        let mut data = profiles_dataset(ProfilesDatasetKind::Cpu, 1, 2, 2);
        let dictionary = data.dictionary.as_mut().unwrap();
        dictionary
            .stack_table
            .push(dictionary.stack_table[1].clone());
        data.resource_profiles[0].scope_profiles[0].profiles[0].samples[1].stack_index = 2;
        let records = encode_profiles_otap_batch(&data).unwrap();
        let selection = BooleanArray::from(vec![false, true]);
        let (filtered, _) =
            filter_profile_samples(&records, &selection, ProfilesTransformLimits::default())
                .unwrap();

        let (compacted, stats) = compact_profile_dimensions(
            &filtered,
            ProfilesCompactionOptions { dense_ids: true },
            ProfilesTransformLimits::default(),
        )
        .unwrap();

        let stack_ids =
            get_u32_array(compacted.get(ArrowPayloadType::Stacks).unwrap(), consts::ID).unwrap();
        let stack_refs = get_u32_array(
            compacted.get(ArrowPayloadType::Samples).unwrap(),
            consts::STACK_ID,
        )
        .unwrap();
        assert_eq!(stack_ids.values(), &[1]);
        assert_eq!(stack_refs.values(), &[1]);
        assert!(stats.rewritten_ids >= 2);
    }

    /// Scenario: A sample-owned attribute is deleted across multiple owners.
    /// Guarantees: The optional attribute payload is removed without affecting sample ownership.
    #[test]
    fn transforms_sample_attributes_and_rebuilds_ordinals() {
        let records = cpu_records(3);
        let transform =
            AttributesTransform::default().with_delete(DeleteTransform::new(BTreeSet::from([
                "profile.kind".to_string(),
            ])));

        let (transformed, stats) = transform_profile_attributes(
            &records,
            ProfilesAttributeOwner::Sample,
            &transform,
            ProfilesTransformLimits::default(),
        )
        .unwrap();

        assert!(
            transformed
                .get(ArrowPayloadType::ProfileSampleAttrs)
                .is_none()
        );
        assert_eq!(stats.renamed_entries, 0);
        assert_eq!(stats.deleted_entries, 3);
        assert_eq!(
            transformed
                .get(ArrowPayloadType::Samples)
                .unwrap()
                .num_rows(),
            3
        );
    }

    /// Scenario: A sample-owned attribute key is renamed for every sample owner.
    /// Guarantees: Collision-safe rename preserves one unique key and contiguous ordinals.
    #[test]
    fn renames_sample_attributes() {
        let records = cpu_records(2);
        let transform = AttributesTransform::default().with_rename(RenameTransform::new(
            BTreeMap::from([("profile.kind".to_string(), "kind".to_string())]),
        ));

        let (transformed, stats) = transform_profile_attributes(
            &records,
            ProfilesAttributeOwner::Sample,
            &transform,
            ProfilesTransformLimits::default(),
        )
        .unwrap();

        let attrs = transformed
            .get(ArrowPayloadType::ProfileSampleAttrs)
            .unwrap();
        let keys =
            StringArrayAccessor::try_new(attrs.column_by_name(consts::ATTRIBUTE_KEY).unwrap())
                .unwrap();
        assert_eq!(stats.renamed_entries, 2);
        for row in 0..attrs.num_rows() {
            assert_eq!(keys.str_at(row), Some("kind"));
        }
    }

    /// Scenario: A Profiles attribute rename uses an empty destination key.
    /// Guarantees: Canonical validation rejects the output before publication.
    #[test]
    fn rejects_empty_attribute_key_rename() {
        let records = cpu_records(1);
        let transform = AttributesTransform::default().with_rename(RenameTransform::new(
            BTreeMap::from([("profile.kind".to_string(), String::new())]),
        ));

        let result = transform_profile_attributes(
            &records,
            ProfilesAttributeOwner::Sample,
            &transform,
            ProfilesTransformLimits::default(),
        );

        assert!(matches!(
            result,
            Err(Error::InvalidProfilesGraph {
                source: crate::views::otap::ProfilesValidationError::EmptyAttributeKey { .. }
            })
        ));
    }

    /// Scenario: Deleting the only attribute would turn a referenced mapping into protobuf zero.
    /// Guarantees: Mapping attribute mutation rejects the canonical-invalid zero entity.
    #[test]
    fn rejects_mapping_attribute_delete_that_creates_zero_mapping() {
        let mut data = profiles_dataset(ProfilesDatasetKind::Cpu, 1, 1, 1);
        let mapping = &mut data.dictionary.as_mut().unwrap().mapping_table[1];
        mapping.memory_start = 0;
        mapping.memory_limit = 0;
        mapping.file_offset = 0;
        mapping.filename_strindex = 0;
        let records = encode_profiles_otap_batch(&data).unwrap();
        let transform =
            AttributesTransform::default().with_delete(DeleteTransform::new(BTreeSet::from([
                "profile.kind".to_string(),
            ])));

        let result = transform_profile_attributes(
            &records,
            ProfilesAttributeOwner::Mapping,
            &transform,
            ProfilesTransformLimits::default(),
        );

        assert!(matches!(
            result,
            Err(Error::InvalidProfilesGraph {
                source: crate::views::otap::ProfilesValidationError::ZeroMapping { .. }
            })
        ));
    }

    /// Scenario: Deleting the only attribute would turn a referenced location into protobuf zero.
    /// Guarantees: Location attribute mutation rejects the canonical-invalid zero entity.
    #[test]
    fn rejects_location_attribute_delete_that_creates_zero_location() {
        let mut data = profiles_dataset(ProfilesDatasetKind::Cpu, 1, 1, 1);
        let location = &mut data.dictionary.as_mut().unwrap().location_table[1];
        location.mapping_index = 0;
        location.address = 0;
        location.lines.clear();
        let records = encode_profiles_otap_batch(&data).unwrap();
        let transform =
            AttributesTransform::default().with_delete(DeleteTransform::new(BTreeSet::from([
                "profile.kind".to_string(),
            ])));

        let result = transform_profile_attributes(
            &records,
            ProfilesAttributeOwner::Location,
            &transform,
            ProfilesTransformLimits::default(),
        );

        assert!(matches!(
            result,
            Err(Error::InvalidProfilesGraph {
                source: crate::views::otap::ProfilesValidationError::ZeroLocation { .. }
            })
        ));
    }

    /// Scenario: A shared-owner attribute is deleted globally from mappings and locations.
    /// Guarantees: Shared entities remain reachable while their complete owner attribute set changes.
    #[test]
    fn transforms_global_mapping_and_location_attributes() {
        let records = cpu_records(1);
        let transform =
            AttributesTransform::default().with_delete(DeleteTransform::new(BTreeSet::from([
                "profile.kind".to_string(),
            ])));

        for (owner, payload_type) in [
            (
                ProfilesAttributeOwner::Mapping,
                ArrowPayloadType::ProfileMappingAttrs,
            ),
            (
                ProfilesAttributeOwner::Location,
                ArrowPayloadType::ProfileLocationAttrs,
            ),
        ] {
            let (transformed, stats) = transform_profile_attributes(
                &records,
                owner,
                &transform,
                ProfilesTransformLimits::default(),
            )
            .unwrap();
            assert!(transformed.get(payload_type).is_none());
            assert!(stats.deleted_entries > 0);
            assert!(transformed.get(owner.parent_payload_type()).is_some());
        }
    }

    /// Scenario: Function filenames are globally redacted in a Profiles BAR.
    /// Guarantees: Every present filename changes while graph IDs and row counts remain stable.
    #[test]
    fn globally_redacts_function_filenames() {
        let records = cpu_records(2);
        let function_rows = records
            .get(ArrowPayloadType::ProfileFunctions)
            .unwrap()
            .num_rows();

        let (redacted, stats) = redact_profile_function_filenames_global(
            &records,
            "[redacted]",
            ProfilesTransformLimits::default(),
        )
        .unwrap();

        let functions = redacted.get(ArrowPayloadType::ProfileFunctions).unwrap();
        let filenames =
            StringArrayAccessor::try_new(functions.column_by_name(consts::FILENAME).unwrap())
                .unwrap();
        assert_eq!(functions.num_rows(), function_rows);
        assert_eq!(stats.functions_redacted, function_rows as u64);
        assert_eq!(stats.cloned_rows, 0);
        for row in 0..functions.num_rows() {
            assert_eq!(filenames.str_at(row), Some("[redacted]"));
        }
    }

    /// Scenario: A function is identified only by its filename and redaction uses an empty marker.
    /// Guarantees: Redaction rejects creation of a prohibited zero function.
    #[test]
    fn rejects_redaction_that_would_zero_a_function() {
        let mut data = profiles_dataset(ProfilesDatasetKind::Cpu, 1, 1, 1);
        data.dictionary.as_mut().unwrap().function_table[1].name_strindex = 0;
        let records = encode_profiles_otap_batch(&data).unwrap();

        let result = redact_profile_function_filenames_global(
            &records,
            "",
            ProfilesTransformLimits::default(),
        );

        assert!(matches!(
            result,
            Err(Error::UnsupportedProfilesTransform {
                operation: "empty filename redaction would create a zero function"
            })
        ));
    }

    /// Scenario: Two samples share one stack and only the first sample is selected for redaction.
    /// Guarantees: Copy-on-write preserves the original path and repoints only the selected sample.
    #[test]
    fn selection_local_redaction_clones_shared_paths() {
        let records = cpu_records(2);
        let original_function_rows = records
            .get(ArrowPayloadType::ProfileFunctions)
            .unwrap()
            .num_rows();
        let original_location_rows = records
            .get(ArrowPayloadType::ProfileLocations)
            .unwrap()
            .num_rows();
        let original_line_rows = records
            .get(ArrowPayloadType::ProfileLocationLines)
            .unwrap()
            .num_rows();
        let original_stack_edge_rows = records
            .get(ArrowPayloadType::StackLocations)
            .unwrap()
            .num_rows();
        let original_location_attr_rows = records
            .get(ArrowPayloadType::ProfileLocationAttrs)
            .unwrap()
            .num_rows();

        let (redacted, stats) = redact_profile_function_filenames_for_samples(
            &records,
            &HashSet::from([1]),
            "[selected]",
            ProfilesTransformLimits::default(),
        )
        .unwrap();

        let samples = redacted.get(ArrowPayloadType::Samples).unwrap();
        let stack_refs = get_u32_array(samples, consts::STACK_ID).unwrap();
        assert_ne!(stack_refs.value(0), stack_refs.value(1));
        assert_eq!(stack_refs.value(1), 1);

        let functions = redacted.get(ArrowPayloadType::ProfileFunctions).unwrap();
        let function_ids = get_u32_array(functions, consts::ID).unwrap();
        let filenames =
            StringArrayAccessor::try_new(functions.column_by_name(consts::FILENAME).unwrap())
                .unwrap();
        assert_eq!(functions.num_rows(), original_function_rows * 2);
        for row in 0..original_function_rows {
            assert_ne!(filenames.str_at(row), Some("[selected]"));
        }
        for row in original_function_rows..functions.num_rows() {
            assert!(function_ids.value(row) > original_function_rows as u32);
            assert_eq!(filenames.str_at(row), Some("[selected]"));
        }
        assert_eq!(
            redacted
                .get(ArrowPayloadType::ProfileLocations)
                .unwrap()
                .num_rows(),
            original_location_rows * 2
        );
        assert_eq!(
            redacted
                .get(ArrowPayloadType::ProfileLocationLines)
                .unwrap()
                .num_rows(),
            original_line_rows * 2
        );
        assert_eq!(
            redacted
                .get(ArrowPayloadType::StackLocations)
                .unwrap()
                .num_rows(),
            original_stack_edge_rows * 2
        );
        assert_eq!(
            redacted
                .get(ArrowPayloadType::ProfileLocationAttrs)
                .unwrap()
                .num_rows(),
            original_location_attr_rows * 2
        );
        assert_eq!(stats.functions_redacted, original_function_rows as u64);
        assert!(stats.cloned_rows > stats.functions_redacted);
    }

    /// Scenario: Selection-local function redaction has a zero cloned-row budget.
    /// Guarantees: The operation fails before allocation and leaves the source graph unchanged.
    #[test]
    fn selection_local_redaction_enforces_clone_limit() {
        let records = cpu_records(2);
        let original = records.clone();
        let result = redact_profile_function_filenames_for_samples(
            &records,
            &HashSet::from([1]),
            "[selected]",
            ProfilesTransformLimits {
                max_cloned_rows: 0,
                ..ProfilesTransformLimits::default()
            },
        );

        assert!(matches!(
            result,
            Err(Error::ProfilesTransformLimitExceeded {
                limit: "selected samples",
                ..
            })
        ));
        assert_eq!(records, original);
    }

    /// Scenario: Copy-on-write would exceed the configured total output-row limit.
    /// Guarantees: Row amplification is rejected before graph rows are cloned.
    #[test]
    fn selection_local_redaction_preflights_output_rows() {
        let records = cpu_records(2);
        let original = records.clone();
        let current_rows = total_rows(&records).unwrap();
        let result = redact_profile_function_filenames_for_samples(
            &records,
            &HashSet::from([1]),
            "[selected]",
            ProfilesTransformLimits {
                max_output_rows: current_rows,
                ..ProfilesTransformLimits::default()
            },
        );

        assert!(matches!(
            result,
            Err(Error::ProfilesTransformLimitExceeded {
                limit: "output rows",
                ..
            })
        ));
        assert_eq!(records, original);
    }

    /// Scenario: A Profiles transform attempts to insert a new sample attribute.
    /// Guarantees: Unsupported ordinal-creating mutation fails without changing the input graph.
    #[test]
    fn rejects_attribute_insertion_without_partial_mutation() {
        let records = cpu_records(1);
        let original = records.clone();
        let transform = AttributesTransform::default().with_insert(
            super::super::InsertTransform::new(BTreeMap::from([(
                "new".to_string(),
                super::super::LiteralValue::Str("value".to_string()),
            )])),
        );

        let result = transform_profile_attributes(
            &records,
            ProfilesAttributeOwner::Sample,
            &transform,
            ProfilesTransformLimits::default(),
        );

        assert!(matches!(
            result,
            Err(Error::UnsupportedProfilesTransform { .. })
        ));
        assert_eq!(records, original);
    }

    /// Scenario: A no-op filter would publish more rows than the configured output budget.
    /// Guarantees: Limit failure returns an error and preserves the original graph.
    #[test]
    fn enforces_output_row_limit_atomically() {
        let records = cpu_records(1);
        let original = records.clone();
        let selection = BooleanArray::from(vec![true]);
        let result = filter_profile_samples(
            &records,
            &selection,
            ProfilesTransformLimits {
                max_output_rows: 0,
                max_output_bytes: usize::MAX,
                max_cloned_rows: usize::MAX,
            },
        );

        assert!(matches!(
            result,
            Err(Error::ProfilesTransformLimitExceeded {
                limit: "output rows",
                ..
            })
        ));
        assert_eq!(records, original);
    }

    /// Scenario: One sample owner contains two attributes with the same key and valid ordinals.
    /// Guarantees: Profiles graph validation rejects duplicate owner keys before transformation.
    #[test]
    fn validation_rejects_duplicate_profile_attribute_keys() {
        let records = cpu_records(1);
        let attrs = records.get(ArrowPayloadType::ProfileSampleAttrs).unwrap();
        let indices = UInt32Array::from(vec![0, 0]);
        let columns = attrs
            .columns()
            .iter()
            .map(|column| take(column, &indices, None))
            .collect::<arrow::error::Result<Vec<_>>>()
            .unwrap();
        let duplicated = RecordBatch::try_new(attrs.schema(), columns).unwrap();
        let duplicated =
            replace_u32_column(duplicated, consts::ORDINAL, vec![Some(0), Some(1)]).unwrap();
        let mut candidate = records.clone();
        candidate
            .set(ArrowPayloadType::ProfileSampleAttrs, duplicated)
            .unwrap();
        let OtapArrowRecords::Profiles(profiles) = candidate else {
            unreachable!()
        };

        let result = Profiles::try_from(profiles.into_raw());
        assert!(matches!(
            result,
            Err(Error::InvalidProfilesGraph {
                source: crate::views::otap::ProfilesValidationError::DuplicateAttributeKey { .. }
            })
        ));
    }

    /// Scenario: A resource owner in a Profiles BAR contains the same attribute key twice.
    /// Guarantees: Shared resource/scope owner validation enforces unique keys.
    #[test]
    fn validation_rejects_duplicate_resource_attribute_keys() {
        let records = cpu_records(1);
        let attrs = records.get(ArrowPayloadType::ResourceAttrs).unwrap();
        let indices = UInt32Array::from(vec![0, 0]);
        let columns = attrs
            .columns()
            .iter()
            .map(|column| take(column, &indices, None))
            .collect::<arrow::error::Result<Vec<_>>>()
            .unwrap();
        let duplicated = RecordBatch::try_new(attrs.schema(), columns).unwrap();
        let mut candidate = records.clone();
        candidate
            .set(ArrowPayloadType::ResourceAttrs, duplicated)
            .unwrap();
        let OtapArrowRecords::Profiles(profiles) = candidate else {
            unreachable!()
        };

        let result = Profiles::try_from(profiles.into_raw());
        assert!(matches!(
            result,
            Err(Error::InvalidProfilesGraph {
                source: crate::views::otap::ProfilesValidationError::DuplicateAttributeKey {
                    payload_type: ArrowPayloadType::ResourceAttrs,
                    ..
                }
            })
        ));
    }

    /// Scenario: Profiles resource attributes and sample parents use transport encodings.
    /// Guarantees: Construction materializes logical owners before uniqueness and graph validation.
    #[test]
    fn profiles_construction_decodes_transport_optimized_parent_ids() {
        let mut data = profiles_dataset(ProfilesDatasetKind::Cpu, 1, 2, 2);
        data.resource_profiles
            .push(data.resource_profiles[0].clone());
        let records = encode_profiles_otap_batch(&data).unwrap();
        let expected_resource_parents = crate::arrays::get_u16_array(
            records.get(ArrowPayloadType::ResourceAttrs).unwrap(),
            consts::PARENT_ID,
        )
        .unwrap()
        .values()
        .to_vec();
        let expected_sample_parents = get_u32_array(
            records.get(ArrowPayloadType::Samples).unwrap(),
            consts::PARENT_ID,
        )
        .unwrap()
        .values()
        .to_vec();
        let mut encoded = records;
        encoded.encode_transport_optimized().unwrap();
        let encoded_sample_parents = get_u32_array(
            encoded.get(ArrowPayloadType::Samples).unwrap(),
            consts::PARENT_ID,
        )
        .unwrap();
        assert_ne!(
            encoded_sample_parents.values().as_ref(),
            expected_sample_parents.as_slice()
        );
        let OtapArrowRecords::Profiles(profiles) = encoded else {
            unreachable!()
        };

        let profiles = Profiles::try_from(profiles.into_raw()).unwrap();
        let resource_parents = crate::arrays::get_u16_array(
            profiles.get(ArrowPayloadType::ResourceAttrs).unwrap(),
            consts::PARENT_ID,
        )
        .unwrap();
        let sample_parents = get_u32_array(
            profiles.get(ArrowPayloadType::Samples).unwrap(),
            consts::PARENT_ID,
        )
        .unwrap();
        assert_eq!(
            resource_parents.values().as_ref(),
            expected_resource_parents.as_slice()
        );
        assert_eq!(
            sample_parents.values().as_ref(),
            expected_sample_parents.as_slice()
        );
    }

    /// Scenario: A Profiles sample parent delta sequence overflows UInt32.
    /// Guarantees: Profiles construction rejects malformed transport data without wrapping IDs.
    #[test]
    fn profiles_construction_rejects_u32_delta_overflow() {
        let records = cpu_records(2);
        let samples = records.get(ArrowPayloadType::Samples).unwrap().clone();
        let samples =
            replace_u32_column(samples, consts::PARENT_ID, vec![Some(2), Some(u32::MAX)]).unwrap();
        let samples = with_column_encoding(samples, consts::PARENT_ID, metadata::encodings::DELTA);
        let mut candidate = records;
        candidate.set(ArrowPayloadType::Samples, samples).unwrap();
        let OtapArrowRecords::Profiles(profiles) = candidate else {
            unreachable!()
        };

        let result = Profiles::try_from(profiles.into_raw());

        assert!(matches!(result, Err(Error::Format { .. })));
    }

    /// Scenario: Profiles resource attribute quasi-deltas overflow a UInt16 owner ID.
    /// Guarantees: Shared-owner decoding rejects overflow instead of aliasing another resource.
    #[test]
    fn profiles_construction_rejects_u16_quasi_delta_overflow() {
        let mut data = profiles_dataset(ProfilesDatasetKind::Cpu, 1, 1, 1);
        data.resource_profiles
            .push(data.resource_profiles[0].clone());
        let records = encode_profiles_otap_batch(&data).unwrap();
        let attrs = records
            .get(ArrowPayloadType::ResourceAttrs)
            .unwrap()
            .clone();
        let attrs = replace_array_column(
            attrs,
            consts::PARENT_ID,
            Arc::new(UInt16Array::from(vec![2, u16::MAX])),
        )
        .unwrap();
        let attrs =
            with_column_encoding(attrs, consts::PARENT_ID, metadata::encodings::QUASI_DELTA);
        let mut candidate = records;
        candidate
            .set(ArrowPayloadType::ResourceAttrs, attrs)
            .unwrap();
        let OtapArrowRecords::Profiles(profiles) = candidate else {
            unreachable!()
        };

        let result = Profiles::try_from(profiles.into_raw());

        assert!(matches!(result, Err(Error::Format { .. })));
    }

    /// Scenario: A Profiles BAR crosses the stateful Producer and Consumer IPC boundary.
    /// Guarantees: Delta-encoded sample parents are materialized exactly once after transport.
    #[test]
    fn profiles_transport_round_trip_materializes_sample_parents_once() {
        let mut records = cpu_records(2);
        let expected = get_u32_array(
            records.get(ArrowPayloadType::Samples).unwrap(),
            consts::PARENT_ID,
        )
        .unwrap()
        .values()
        .to_vec();
        let mut producer = crate::Producer::new();
        let mut bar = producer.produce_bar(&mut records).unwrap();
        let mut consumer = crate::Consumer::default();
        let messages = consumer.consume_bar(&mut bar).unwrap();
        let sample_message = messages
            .iter()
            .find(|message| message.payload_type == ArrowPayloadType::Samples)
            .unwrap();
        let encoded = get_u32_array(&sample_message.record, consts::PARENT_ID).unwrap();
        assert_ne!(encoded.values().as_ref(), expected.as_slice());

        let profiles = crate::otap::from_record_messages::<Profiles>(messages).unwrap();
        let parents = get_u32_array(
            profiles.get(ArrowPayloadType::Samples).unwrap(),
            consts::PARENT_ID,
        )
        .unwrap();
        assert_eq!(parents.values().as_ref(), expected.as_slice());
    }
}
