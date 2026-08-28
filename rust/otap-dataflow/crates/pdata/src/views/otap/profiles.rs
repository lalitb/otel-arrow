// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Zero-copy access and reference-graph validation for OTAP Profiles batches.

use std::collections::{HashMap, HashSet};

use arrow::array::{Array, LargeListArray, RecordBatch, UInt8Array, UInt32Array};
use arrow::datatypes::{Int64Type, UInt64Type};

use crate::arrays::{NullableArrayAccessor, UInt32ArrayAccessor};
use crate::proto::opentelemetry::arrow::v1::ArrowPayloadType;
use crate::schema::{consts, payloads};

const PROFILE_PAYLOAD_COUNT: usize = 14;

/// A schema-checked, zero-copy view over the record batches in one Profiles BAR.
///
/// This type intentionally accepts a slice of payload/batch pairs. Profiles is
/// not added to the engine-wide signal enum until the next implementation layer.
pub struct ProfilesBatchView<'a> {
    batches: [Option<&'a RecordBatch>; PROFILE_PAYLOAD_COUNT],
}

/// A structural or semantic Profiles graph validation error.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProfilesValidationError {
    /// A payload type is not part of the Profiles signal.
    #[error("payload {payload_type:?} is not a Profiles payload")]
    UnsupportedPayload {
        /// The unexpected payload type.
        payload_type: ArrowPayloadType,
    },
    /// A payload type occurs more than once in the input.
    #[error("duplicate payload {payload_type:?}")]
    DuplicatePayload {
        /// The repeated payload type.
        payload_type: ArrowPayloadType,
    },
    /// A required payload is absent.
    #[error("missing required payload {payload_type:?}")]
    MissingPayload {
        /// The absent payload type.
        payload_type: ArrowPayloadType,
    },
    /// A record batch does not match its declared schema.
    #[error("invalid schema for {payload_type:?}: {message}")]
    InvalidSchema {
        /// The payload being checked.
        payload_type: ArrowPayloadType,
        /// The underlying schema error.
        message: String,
    },
    /// A required typed column could not be accessed.
    #[error("invalid column {column} in {payload_type:?}: {message}")]
    InvalidColumn {
        /// The payload containing the column.
        payload_type: ArrowPayloadType,
        /// The column name.
        column: &'static str,
        /// The access error.
        message: String,
    },
    /// An entity ID is zero or occurs more than once.
    #[error("invalid {column} {id} in {payload_type:?}: {reason}")]
    InvalidId {
        /// The payload containing the ID.
        payload_type: ArrowPayloadType,
        /// The ID column.
        column: &'static str,
        /// The invalid ID.
        id: u32,
        /// Why it is invalid.
        reason: &'static str,
    },
    /// A foreign key does not resolve.
    #[error("unresolved {column} {id} in {payload_type:?}")]
    UnresolvedReference {
        /// The referencing payload.
        payload_type: ArrowPayloadType,
        /// The foreign-key column.
        column: &'static str,
        /// The unresolved ID.
        id: u32,
    },
    /// An ordered child key is duplicated or non-contiguous.
    #[error("invalid ordinal {ordinal} for parent {parent_id} in {payload_type:?}")]
    InvalidOrdinal {
        /// The child payload.
        payload_type: ArrowPayloadType,
        /// The owner ID.
        parent_id: u32,
        /// The invalid ordinal.
        ordinal: u32,
    },
    /// A value type role is outside its domain or duplicated for one profile.
    #[error("invalid value type role {role} for profile {parent_id}")]
    InvalidValueTypeRole {
        /// The owning profile.
        parent_id: u32,
        /// The invalid or duplicated role.
        role: u8,
    },
    /// A sample's observation lists violate the Profiles relationship rules.
    #[error("invalid observation lists for sample {sample_id}")]
    InvalidObservations {
        /// The sample containing the lists.
        sample_id: u32,
    },
}

impl<'a> ProfilesBatchView<'a> {
    /// Build a view and validate every included batch against its Arrow schema.
    pub fn try_new(
        batches: &'a [(ArrowPayloadType, RecordBatch)],
    ) -> Result<Self, ProfilesValidationError> {
        let mut view = Self {
            batches: [None; PROFILE_PAYLOAD_COUNT],
        };

        for (payload_type, batch) in batches {
            let Some(index) = payload_index(*payload_type) else {
                return Err(ProfilesValidationError::UnsupportedPayload {
                    payload_type: *payload_type,
                });
            };
            if view.batches[index].is_some() {
                return Err(ProfilesValidationError::DuplicatePayload {
                    payload_type: *payload_type,
                });
            }
            payloads::get(*payload_type)
                .check_match(batch)
                .map_err(|error| ProfilesValidationError::InvalidSchema {
                    payload_type: *payload_type,
                    message: error.to_string(),
                })?;
            view.batches[index] = Some(batch);
        }

        for payload_type in [ArrowPayloadType::Profiles, ArrowPayloadType::Samples] {
            if view.get(payload_type).is_none() {
                return Err(ProfilesValidationError::MissingPayload { payload_type });
            }
        }

        Ok(view)
    }

    /// Return one Profiles record batch without copying its Arrow buffers.
    #[must_use]
    pub fn get(&self, payload_type: ArrowPayloadType) -> Option<&'a RecordBatch> {
        payload_index(payload_type).and_then(|index| self.batches[index])
    }

    /// Validate IDs, foreign keys, ordered children, and sample observations.
    pub fn validate_graph(&self) -> Result<(), ProfilesValidationError> {
        let profile_ids = self.entity_ids(ArrowPayloadType::Profiles)?;
        let sample_ids = self.entity_ids(ArrowPayloadType::Samples)?;
        let stack_ids = self.optional_entity_ids(ArrowPayloadType::Stacks)?;
        let location_ids = self.optional_entity_ids(ArrowPayloadType::ProfileLocations)?;
        let function_ids = self.optional_entity_ids(ArrowPayloadType::ProfileFunctions)?;
        let mapping_ids = self.optional_entity_ids(ArrowPayloadType::ProfileMappings)?;
        let link_ids = self.optional_entity_ids(ArrowPayloadType::ProfileLinks)?;

        self.validate_parent_refs(ArrowPayloadType::Samples, &profile_ids)?;
        self.validate_parent_refs(ArrowPayloadType::ProfileValueTypes, &profile_ids)?;
        self.validate_parent_refs(ArrowPayloadType::ProfileAttrs, &profile_ids)?;
        self.validate_parent_refs(ArrowPayloadType::StackLocations, &stack_ids)?;
        self.validate_parent_refs(ArrowPayloadType::ProfileLocationLines, &location_ids)?;
        self.validate_parent_refs(ArrowPayloadType::ProfileSampleAttrs, &sample_ids)?;
        self.validate_parent_refs(ArrowPayloadType::ProfileMappingAttrs, &mapping_ids)?;
        self.validate_parent_refs(ArrowPayloadType::ProfileLocationAttrs, &location_ids)?;

        self.validate_optional_refs(ArrowPayloadType::Samples, consts::STACK_ID, &stack_ids)?;
        self.validate_optional_refs(ArrowPayloadType::Samples, consts::LINK_ID, &link_ids)?;
        self.validate_optional_refs(
            ArrowPayloadType::StackLocations,
            consts::LOCATION_ID,
            &location_ids,
        )?;
        self.validate_optional_refs(
            ArrowPayloadType::ProfileLocations,
            consts::MAPPING_ID,
            &mapping_ids,
        )?;
        self.validate_optional_refs(
            ArrowPayloadType::ProfileLocationLines,
            consts::FUNCTION_ID,
            &function_ids,
        )?;

        for payload_type in [
            ArrowPayloadType::StackLocations,
            ArrowPayloadType::ProfileLocationLines,
            ArrowPayloadType::ProfileAttrs,
            ArrowPayloadType::ProfileSampleAttrs,
            ArrowPayloadType::ProfileMappingAttrs,
            ArrowPayloadType::ProfileLocationAttrs,
        ] {
            self.validate_ordinals(payload_type)?;
        }

        self.validate_value_type_roles()?;
        self.validate_observations()?;
        Ok(())
    }

    fn entity_ids(
        &self,
        payload_type: ArrowPayloadType,
    ) -> Result<HashSet<u32>, ProfilesValidationError> {
        let batch = self
            .get(payload_type)
            .ok_or(ProfilesValidationError::MissingPayload { payload_type })?;
        unique_ids(batch, payload_type, consts::ID)
    }

    fn optional_entity_ids(
        &self,
        payload_type: ArrowPayloadType,
    ) -> Result<HashSet<u32>, ProfilesValidationError> {
        self.get(payload_type)
            .map(|batch| unique_ids(batch, payload_type, consts::ID))
            .unwrap_or_else(|| Ok(HashSet::new()))
    }

    fn validate_parent_refs(
        &self,
        payload_type: ArrowPayloadType,
        targets: &HashSet<u32>,
    ) -> Result<(), ProfilesValidationError> {
        let Some(batch) = self.get(payload_type) else {
            return Ok(());
        };
        let parents = required_u32(batch, payload_type, consts::PARENT_ID)?;
        for row in 0..parents.len() {
            let id = parents.value_at(row).unwrap_or_default();
            if id == 0 || !targets.contains(&id) {
                return Err(ProfilesValidationError::UnresolvedReference {
                    payload_type,
                    column: consts::PARENT_ID,
                    id,
                });
            }
        }
        Ok(())
    }

    fn validate_optional_refs(
        &self,
        payload_type: ArrowPayloadType,
        column: &'static str,
        targets: &HashSet<u32>,
    ) -> Result<(), ProfilesValidationError> {
        let Some(batch) = self.get(payload_type) else {
            return Ok(());
        };
        let Some(array) = batch.column_by_name(column) else {
            return Ok(());
        };
        let ids = array
            .as_any()
            .downcast_ref::<UInt32Array>()
            .ok_or_else(|| ProfilesValidationError::InvalidColumn {
                payload_type,
                column,
                message: format!("expected UInt32, got {}", array.data_type()),
            })?;
        for row in 0..ids.len() {
            if let Some(id) = ids.value_at(row)
                && (id == 0 || !targets.contains(&id))
            {
                return Err(ProfilesValidationError::UnresolvedReference {
                    payload_type,
                    column,
                    id,
                });
            }
        }
        Ok(())
    }

    fn validate_ordinals(
        &self,
        payload_type: ArrowPayloadType,
    ) -> Result<(), ProfilesValidationError> {
        let Some(batch) = self.get(payload_type) else {
            return Ok(());
        };
        let parents = required_u32(batch, payload_type, consts::PARENT_ID)?;
        let ordinals = native_u32(batch, payload_type, consts::ORDINAL)?;
        let mut next_by_parent = HashMap::new();
        for row in 0..batch.num_rows() {
            let parent_id = parents.value_at(row).unwrap_or_default();
            let ordinal = ordinals.value(row);
            let next = next_by_parent.entry(parent_id).or_insert(0);
            if ordinal != *next {
                return Err(ProfilesValidationError::InvalidOrdinal {
                    payload_type,
                    parent_id,
                    ordinal,
                });
            }
            *next += 1;
        }
        Ok(())
    }

    fn validate_value_type_roles(&self) -> Result<(), ProfilesValidationError> {
        let payload_type = ArrowPayloadType::ProfileValueTypes;
        let Some(batch) = self.get(payload_type) else {
            return Ok(());
        };
        let parents = required_u32(batch, payload_type, consts::PARENT_ID)?;
        let roles = batch
            .column_by_name(consts::ROLE)
            .and_then(|array| array.as_any().downcast_ref::<UInt8Array>())
            .ok_or_else(|| ProfilesValidationError::InvalidColumn {
                payload_type,
                column: consts::ROLE,
                message: "expected UInt8".to_string(),
            })?;
        let mut seen = HashSet::new();
        for row in 0..batch.num_rows() {
            let parent_id = parents.value_at(row).unwrap_or_default();
            let role = roles.value(row);
            if role > 1 || !seen.insert((parent_id, role)) {
                return Err(ProfilesValidationError::InvalidValueTypeRole { parent_id, role });
            }
        }
        Ok(())
    }

    fn validate_observations(&self) -> Result<(), ProfilesValidationError> {
        let payload_type = ArrowPayloadType::Samples;
        let batch = self.get(payload_type).expect("checked in try_new");
        let ids = native_u32(batch, payload_type, consts::ID)?;
        let values = large_list_i64(batch, payload_type, consts::VALUES)?;
        let timestamps = large_list_u64(batch, payload_type, consts::TIMESTAMPS_UNIX_NANO)?;
        for row in 0..batch.num_rows() {
            let values_len = values.value_length(row);
            let timestamps_len = timestamps.value_length(row);
            if (values_len == 0 && timestamps_len == 0)
                || (values_len != 0 && timestamps_len != 0 && values_len != timestamps_len)
            {
                return Err(ProfilesValidationError::InvalidObservations {
                    sample_id: ids.value(row),
                });
            }
        }
        Ok(())
    }
}

fn payload_index(payload_type: ArrowPayloadType) -> Option<usize> {
    match payload_type {
        ArrowPayloadType::Profiles => Some(0),
        ArrowPayloadType::ProfileValueTypes => Some(1),
        ArrowPayloadType::Samples => Some(2),
        ArrowPayloadType::Stacks => Some(3),
        ArrowPayloadType::StackLocations => Some(4),
        ArrowPayloadType::ProfileLocations => Some(5),
        ArrowPayloadType::ProfileLocationLines => Some(6),
        ArrowPayloadType::ProfileFunctions => Some(7),
        ArrowPayloadType::ProfileMappings => Some(8),
        ArrowPayloadType::ProfileLinks => Some(9),
        ArrowPayloadType::ProfileAttrs => Some(10),
        ArrowPayloadType::ProfileSampleAttrs => Some(11),
        ArrowPayloadType::ProfileMappingAttrs => Some(12),
        ArrowPayloadType::ProfileLocationAttrs => Some(13),
        _ => None,
    }
}

fn unique_ids(
    batch: &RecordBatch,
    payload_type: ArrowPayloadType,
    column: &'static str,
) -> Result<HashSet<u32>, ProfilesValidationError> {
    let ids = native_u32(batch, payload_type, column)?;
    let mut result = HashSet::with_capacity(ids.len());
    for id in ids.values() {
        if *id == 0 || !result.insert(*id) {
            return Err(ProfilesValidationError::InvalidId {
                payload_type,
                column,
                id: *id,
                reason: if *id == 0 { "zero" } else { "duplicate" },
            });
        }
    }
    Ok(result)
}

fn required_u32<'a>(
    batch: &'a RecordBatch,
    payload_type: ArrowPayloadType,
    column: &'static str,
) -> Result<UInt32ArrayAccessor<'a>, ProfilesValidationError> {
    UInt32ArrayAccessor::try_new_for_column(batch, column).map_err(|error| {
        ProfilesValidationError::InvalidColumn {
            payload_type,
            column,
            message: error.to_string(),
        }
    })
}

fn native_u32<'a>(
    batch: &'a RecordBatch,
    payload_type: ArrowPayloadType,
    column: &'static str,
) -> Result<&'a UInt32Array, ProfilesValidationError> {
    batch
        .column_by_name(column)
        .and_then(|array| array.as_any().downcast_ref::<UInt32Array>())
        .ok_or_else(|| ProfilesValidationError::InvalidColumn {
            payload_type,
            column,
            message: "expected UInt32".to_string(),
        })
}

fn large_list_i64<'a>(
    batch: &'a RecordBatch,
    payload_type: ArrowPayloadType,
    column: &'static str,
) -> Result<&'a LargeListArray, ProfilesValidationError> {
    large_list::<Int64Type>(batch, payload_type, column)
}

fn large_list_u64<'a>(
    batch: &'a RecordBatch,
    payload_type: ArrowPayloadType,
    column: &'static str,
) -> Result<&'a LargeListArray, ProfilesValidationError> {
    large_list::<UInt64Type>(batch, payload_type, column)
}

fn large_list<'a, T>(
    batch: &'a RecordBatch,
    payload_type: ArrowPayloadType,
    column: &'static str,
) -> Result<&'a LargeListArray, ProfilesValidationError>
where
    T: arrow::datatypes::ArrowPrimitiveType,
{
    let list = batch
        .column_by_name(column)
        .and_then(|array| array.as_any().downcast_ref::<LargeListArray>())
        .ok_or_else(|| ProfilesValidationError::InvalidColumn {
            payload_type,
            column,
            message: "expected LargeList".to_string(),
        })?;
    if list.values().data_type() != &T::DATA_TYPE {
        return Err(ProfilesValidationError::InvalidColumn {
            payload_type,
            column,
            message: format!("expected LargeList({})", T::DATA_TYPE),
        });
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::{ArrayRef, Int64Array, UInt64Array};
    use arrow::buffer::{OffsetBuffer, ScalarBuffer};
    use arrow::datatypes::{DataType, Field, Schema};

    use super::*;

    fn root_batch(ids: Vec<u32>) -> RecordBatch {
        let len = ids.len();
        RecordBatch::try_from_iter([
            (consts::ID, Arc::new(UInt32Array::from(ids)) as ArrayRef),
            (
                consts::TIME_UNIX_NANO,
                Arc::new(UInt64Array::from(vec![1; len])) as ArrayRef,
            ),
            (
                consts::DURATION_NANO,
                Arc::new(UInt64Array::from(vec![1; len])) as ArrayRef,
            ),
        ])
        .unwrap()
    }

    fn large_list_i64(rows: &[&[i64]]) -> LargeListArray {
        let lengths = rows.iter().map(|row| row.len());
        let values: Vec<i64> = rows.iter().flat_map(|row| row.iter().copied()).collect();
        LargeListArray::new(
            Arc::new(Field::new("item", DataType::Int64, true)),
            OffsetBuffer::from_lengths(lengths),
            Arc::new(Int64Array::new(ScalarBuffer::from(values), None)),
            None,
        )
    }

    fn large_list_u64(rows: &[&[u64]]) -> LargeListArray {
        let lengths = rows.iter().map(|row| row.len());
        let values: Vec<u64> = rows.iter().flat_map(|row| row.iter().copied()).collect();
        LargeListArray::new(
            Arc::new(Field::new("item", DataType::UInt64, true)),
            OffsetBuffer::from_lengths(lengths),
            Arc::new(UInt64Array::new(ScalarBuffer::from(values), None)),
            None,
        )
    }

    fn samples_batch(
        ids: Vec<u32>,
        parents: Vec<u32>,
        values: &[&[i64]],
        times: &[&[u64]],
    ) -> RecordBatch {
        let fields = vec![
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
        ];
        RecordBatch::try_new(
            Arc::new(Schema::new(fields)),
            vec![
                Arc::new(UInt32Array::from(ids)),
                Arc::new(UInt32Array::from(parents)),
                Arc::new(large_list_i64(values)),
                Arc::new(large_list_u64(times)),
            ],
        )
        .unwrap()
    }

    fn minimal_batches(
        sample_parent: u32,
        values: &[&[i64]],
        times: &[&[u64]],
    ) -> Vec<(ArrowPayloadType, RecordBatch)> {
        vec![
            (ArrowPayloadType::Profiles, root_batch(vec![1])),
            (
                ArrowPayloadType::Samples,
                samples_batch(vec![10], vec![sample_parent], values, times),
            ),
        ]
    }

    /// Scenario: A minimal Profiles BAR has one profile and one timestamped sample.
    /// Guarantees: Schema and graph validation accept valid zero-copy input batches.
    #[test]
    fn accepts_minimal_valid_graph() {
        let batches = minimal_batches(1, &[&[7]], &[&[11]]);
        let view = ProfilesBatchView::try_new(&batches).unwrap();
        view.validate_graph().unwrap();
        assert!(std::ptr::eq(
            view.get(ArrowPayloadType::Profiles).unwrap(),
            &batches[0].1
        ));
    }

    /// Scenario: A sample refers to a profile ID absent from the root batch.
    /// Guarantees: Graph validation rejects dangling parent references.
    #[test]
    fn rejects_dangling_sample_parent() {
        let batches = minimal_batches(2, &[&[7]], &[&[11]]);
        let view = ProfilesBatchView::try_new(&batches).unwrap();
        assert_eq!(
            view.validate_graph(),
            Err(ProfilesValidationError::UnresolvedReference {
                payload_type: ArrowPayloadType::Samples,
                column: consts::PARENT_ID,
                id: 2,
            })
        );
    }

    /// Scenario: Both observation lists in a sample are empty.
    /// Guarantees: A sample without any measured values or timestamps is rejected.
    #[test]
    fn rejects_empty_observation_lists() {
        let batches = minimal_batches(1, &[&[]], &[&[]]);
        let view = ProfilesBatchView::try_new(&batches).unwrap();
        assert_eq!(
            view.validate_graph(),
            Err(ProfilesValidationError::InvalidObservations { sample_id: 10 })
        );
    }

    /// Scenario: Two root profile rows reuse the same nonzero BAR-local ID.
    /// Guarantees: Entity IDs remain unique within their record batch.
    #[test]
    fn rejects_duplicate_profile_ids() {
        let batches = vec![
            (ArrowPayloadType::Profiles, root_batch(vec![1, 1])),
            (
                ArrowPayloadType::Samples,
                samples_batch(vec![10], vec![1], &[&[7]], &[&[11]]),
            ),
        ];
        let view = ProfilesBatchView::try_new(&batches).unwrap();
        assert_eq!(
            view.validate_graph(),
            Err(ProfilesValidationError::InvalidId {
                payload_type: ArrowPayloadType::Profiles,
                column: consts::ID,
                id: 1,
                reason: "duplicate",
            })
        );
    }
}
