// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Zero-copy view traits implementation for OTAP Arrow RecordBatches.
//!
//! This module provides efficient traversal of OTAP Arrow data using the unified
//! view trait system. All string access is zero-copy via dictionary encoding.
//!
//! # Architecture
//!
//! ```text
//! ArrowLogsData (owns cursors, borrows batches)
//! └─ ArrowResourceLogsIter
//!    └─ ArrowResourceLogs
//!       └─ ArrowScopeLogsIter
//!          └─ ArrowScopeLogs
//!             └─ ArrowLogRecordIter
//!                └─ ArrowLogRecord
//!                   ├─ ArrowAttributeIter → ArrowAttribute
//!                   └─ ArrowAnyValue (body)
//! ```
//!
//! # Performance Characteristics
//!
//! - **Zero-copy strings**: All `&str` access borrows directly from Arrow dictionaries
//! - **Sorted traversal**: O(1) attribute lookup per log via cursor (early termination)
//! - **Single allocation**: Cursors initialized once, reused across all iterations
//! - **Cache-friendly**: Sequential access patterns within Arrow columns

use std::cell::RefCell;
use std::marker::PhantomData;

use arrow::array::RecordBatch;
use otel_arrow_rust::otlp::attributes::Attribute16Arrays;
use otel_arrow_rust::otlp::logs::LogsArrays;
use otel_arrow_rust::otlp::{BatchSorter, NullableArrayAccessor, SortedBatchCursor};
use otel_arrow_rust::schema::{SpanId, TraceId};

use crate::views::common::{AnyValueView, AttributeView, InstrumentationScopeView, Str, ValueType};
use crate::views::logs::{LogRecordView, LogsDataView, ResourceLogsView, ScopeLogsView};
use crate::views::resource::ResourceView;

// ============================================================================
// Top-Level: ArrowLogsData
// ============================================================================

/// Zero-copy view into OTAP Arrow log data.
///
/// This struct wraps Arrow RecordBatches and manages internal cursor state
/// for efficient parent-child iteration. Cursors are hidden from external users.
///
/// # Example
///
/// ```ignore
/// let arrow_logs = ArrowLogsData::new(logs_batch, log_attrs_batch, resource_attrs_batch)?;
///
/// for resource_logs in arrow_logs.resources() {
///     for scope_logs in resource_logs.scopes() {
///         for log in scope_logs.log_records() {
///             // Zero-copy access!
///             if let Some(body) = log.body() {
///                 if let Some(s) = body.as_string() {
///                     println!("Body: {}", std::str::from_utf8(s).unwrap());
///                 }
///             }
///         }
///     }
/// }
/// ```
pub struct ArrowLogsData<'a> {
    /// Parsed accessor for log record fields (id, time, severity, trace_id, etc.)
    logs_arrays: LogsArrays<'a>,

    /// Optional accessor for log-level attributes (parent_id → log.id)
    log_attrs_arrays: Option<Attribute16Arrays<'a>>,

    /// Optional accessor for resource-level attributes
    resource_attrs_arrays: Option<Attribute16Arrays<'a>>,

    /// Cursor state for log attributes (reused across iterations)
    /// RefCell allows interior mutability for cursor updates during iteration
    log_attrs_cursor: RefCell<SortedBatchCursor>,

    /// Cursor state for resource attributes
    #[allow(dead_code)]
    resource_attrs_cursor: RefCell<SortedBatchCursor>,

    /// Batch sorter (stateless utility, shared across cursors)
    #[allow(dead_code)]
    batch_sorter: BatchSorter,
}

impl<'a> ArrowLogsData<'a> {
    /// Create a new `ArrowLogsData` from Arrow RecordBatches.
    ///
    /// # Arguments
    ///
    /// * `logs_batch` - RecordBatch containing log records (id, time, severity, etc.)
    /// * `log_attrs_batch` - Optional RecordBatch with log-level attributes
    /// * `resource_attrs_batch` - Optional RecordBatch with resource-level attributes
    ///
    /// # Errors
    ///
    /// Returns error if batch schemas don't match expected OTAP schema.
    pub fn new(
        logs_batch: &'a RecordBatch,
        log_attrs_batch: Option<&'a RecordBatch>,
        resource_attrs_batch: Option<&'a RecordBatch>,
    ) -> Result<Self, String> {
        // Parse logs batch into typed accessors
        let logs_arrays = LogsArrays::try_from(logs_batch)
            .map_err(|e| format!("Failed to parse logs batch: {}", e))?;

        // Parse log attributes batch (if present)
        let log_attrs_arrays = log_attrs_batch
            .map(Attribute16Arrays::try_from)
            .transpose()
            .map_err(|e| format!("Failed to parse log attributes batch: {}", e))?;

        // Parse resource attributes batch (if present)
        let resource_attrs_arrays = resource_attrs_batch
            .map(Attribute16Arrays::try_from)
            .transpose()
            .map_err(|e| format!("Failed to parse resource attributes batch: {}", e))?;

        // Initialize cursors
        let mut batch_sorter = BatchSorter::new();
        let mut log_attrs_cursor = SortedBatchCursor::new();
        let mut resource_attrs_cursor = SortedBatchCursor::new();

        // Sort attribute batches once for efficient iteration
        if let Some(ref attrs) = log_attrs_arrays {
            batch_sorter.init_cursor_for_u16_id_column(&attrs.parent_id, &mut log_attrs_cursor);
        }
        if let Some(ref attrs) = resource_attrs_arrays {
            batch_sorter.init_cursor_for_u16_id_column(&attrs.parent_id, &mut resource_attrs_cursor);
        }

        Ok(Self {
            logs_arrays,
            log_attrs_arrays,
            resource_attrs_arrays,
            log_attrs_cursor: RefCell::new(log_attrs_cursor),
            resource_attrs_cursor: RefCell::new(resource_attrs_cursor),
            batch_sorter,
        })
    }

    /// Access the underlying LogsArrays for advanced use cases.
    #[inline]
    pub fn logs_arrays(&self) -> &LogsArrays<'a> {
        &self.logs_arrays
    }

    /// Access the log attributes arrays for advanced use cases.
    #[inline]
    pub fn log_attrs_arrays(&self) -> Option<&Attribute16Arrays<'a>> {
        self.log_attrs_arrays.as_ref()
    }

    /// Access the resource attributes arrays for advanced use cases.
    #[inline]
    pub fn resource_attrs_arrays(&self) -> Option<&Attribute16Arrays<'a>> {
        self.resource_attrs_arrays.as_ref()
    }
}

impl<'a> LogsDataView for ArrowLogsData<'a> {
    type ResourceLogs<'res> = ArrowResourceLogs<'res> where Self: 'res;
    type ResourcesIter<'res> = ArrowResourceLogsIter<'res> where Self: 'res;

    fn resources(&self) -> Self::ResourcesIter<'_> {
        // OTAP currently has a single resource per batch
        ArrowResourceLogsIter {
            parent: self,
            yielded: false,
        }
    }
}

// ============================================================================
// Resource Level
// ============================================================================

/// Iterator over resource logs (currently yields single resource for OTAP).
pub struct ArrowResourceLogsIter<'a> {
    parent: &'a ArrowLogsData<'a>,
    yielded: bool,
}

impl<'a> Iterator for ArrowResourceLogsIter<'a> {
    type Item = ArrowResourceLogs<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.yielded {
            None
        } else {
            self.yielded = true;
            Some(ArrowResourceLogs { parent: self.parent })
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = if self.yielded { 0 } else { 1 };
        (remaining, Some(remaining))
    }
}

impl<'a> ExactSizeIterator for ArrowResourceLogsIter<'a> {}

/// View for resource-level logs.
pub struct ArrowResourceLogs<'a> {
    parent: &'a ArrowLogsData<'a>,
}

impl<'a> ResourceLogsView for ArrowResourceLogs<'a> {
    type Resource<'res> = ArrowResource<'res> where Self: 'res;
    type ScopeLogs<'scp> = ArrowScopeLogs<'scp> where Self: 'scp;
    type ScopesIter<'scp> = ArrowScopeLogsIter<'scp> where Self: 'scp;

    fn resource(&self) -> Option<Self::Resource<'_>> {
        // Return resource view if we have resource attributes
        if self.parent.resource_attrs_arrays.is_some() {
            Some(ArrowResource { parent: self.parent })
        } else {
            None
        }
    }

    fn scopes(&self) -> Self::ScopesIter<'_> {
        // OTAP currently has a single scope per resource
        ArrowScopeLogsIter {
            parent: self.parent,
            yielded: false,
        }
    }

    fn schema_url(&self) -> Option<Str<'_>> {
        // Access schema_url from logs_arrays if present
        self.parent
            .logs_arrays
            .schema_url
            .as_ref()
            .and_then(|accessor| accessor.str_at(0))
            .map(|s| s.as_bytes())
    }
}

// ============================================================================
// Resource View
// ============================================================================

/// View for resource attributes.
pub struct ArrowResource<'a> {
    parent: &'a ArrowLogsData<'a>,
}

impl<'a> ResourceView for ArrowResource<'a> {
    type Attribute<'att> = ArrowAttribute<'att> where Self: 'att;
    type AttributesIter<'att> = ArrowResourceAttributeIter<'att> where Self: 'att;

    fn attributes(&self) -> Self::AttributesIter<'_> {
        ArrowResourceAttributeIter {
            parent: self.parent,
            index: 0,
            // Resource attributes have parent_id = 0 (implicit)
        }
    }

    fn dropped_attributes_count(&self) -> u32 {
        // OTAP doesn't currently track dropped counts
        0
    }
}

/// Iterator over resource attributes.
pub struct ArrowResourceAttributeIter<'a> {
    parent: &'a ArrowLogsData<'a>,
    index: usize,
}

impl<'a> Iterator for ArrowResourceAttributeIter<'a> {
    type Item = ArrowAttribute<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let attrs = self.parent.resource_attrs_arrays.as_ref()?;

        // Resource attributes typically have parent_id = 0
        // Get the total number of attributes from attr_type array (always present)
        let total_attrs = attrs.anyval_arrays.attr_type.len();

        if self.index < total_attrs {
            let attr = ArrowAttribute {
                attrs,
                index: self.index,
            };
            self.index += 1;
            Some(attr)
        } else {
            None
        }
    }
}

// ============================================================================
// Scope Level
// ============================================================================

/// Iterator over scope logs (currently yields single scope for OTAP).
pub struct ArrowScopeLogsIter<'a> {
    parent: &'a ArrowLogsData<'a>,
    yielded: bool,
}

impl<'a> Iterator for ArrowScopeLogsIter<'a> {
    type Item = ArrowScopeLogs<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.yielded {
            None
        } else {
            self.yielded = true;
            Some(ArrowScopeLogs { parent: self.parent })
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = if self.yielded { 0 } else { 1 };
        (remaining, Some(remaining))
    }
}

impl<'a> ExactSizeIterator for ArrowScopeLogsIter<'a> {}

/// View for scope-level logs.
pub struct ArrowScopeLogs<'a> {
    parent: &'a ArrowLogsData<'a>,
}

impl<'a> ScopeLogsView for ArrowScopeLogs<'a> {
    type Scope<'scp> = ArrowInstrumentationScope<'scp> where Self: 'scp;
    type LogRecord<'rec> = ArrowLogRecord<'rec> where Self: 'rec;
    type LogRecordsIter<'rec> = ArrowLogRecordIter<'rec> where Self: 'rec;

    fn scope(&self) -> Option<Self::Scope<'_>> {
        // Return scope view (even if empty, to be consistent with OTLP)
        Some(ArrowInstrumentationScope { parent: self.parent })
    }

    fn log_records(&self) -> Self::LogRecordsIter<'_> {
        ArrowLogRecordIter {
            parent: self.parent,
            index: 0,
        }
    }

    fn schema_url(&self) -> Option<Str<'_>> {
        // Same as resource-level schema_url for OTAP
        self.parent
            .logs_arrays
            .schema_url
            .as_ref()
            .and_then(|accessor| accessor.str_at(0))
            .map(|s| s.as_bytes())
    }
}

// ============================================================================
// InstrumentationScope View
// ============================================================================

/// View for instrumentation scope (currently minimal in OTAP).
pub struct ArrowInstrumentationScope<'a> {
    #[allow(dead_code)]
    parent: &'a ArrowLogsData<'a>,
}

impl<'a> InstrumentationScopeView for ArrowInstrumentationScope<'a> {
    type Attribute<'att> = ArrowAttribute<'att> where Self: 'att;
    type AttributeIter<'att> = ArrowScopeAttributeIter<'att> where Self: 'att;

    fn name(&self) -> Option<Str<'_>> {
        // OTAP doesn't currently store scope name separately
        None
    }

    fn version(&self) -> Option<Str<'_>> {
        // OTAP doesn't currently store scope version
        None
    }

    fn attributes(&self) -> Self::AttributeIter<'_> {
        // No scope attributes in current OTAP schema
        ArrowScopeAttributeIter { _phantom: PhantomData }
    }

    fn dropped_attributes_count(&self) -> u32 {
        0
    }
}

/// Empty iterator for scope attributes (OTAP doesn't have scope attrs currently).
pub struct ArrowScopeAttributeIter<'a> {
    _phantom: PhantomData<&'a ()>,
}

impl<'a> Iterator for ArrowScopeAttributeIter<'a> {
    type Item = ArrowAttribute<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        None
    }
}

// ============================================================================
// LogRecord Level
// ============================================================================

/// Iterator over log records in the batch.
pub struct ArrowLogRecordIter<'a> {
    parent: &'a ArrowLogsData<'a>,
    index: usize,
}

impl<'a> Iterator for ArrowLogRecordIter<'a> {
    type Item = ArrowLogRecord<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index < self.parent.logs_arrays.id.len() {
            let log = ArrowLogRecord {
                parent: self.parent,
                index: self.index,
            };
            self.index += 1;
            Some(log)
        } else {
            None
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.parent.logs_arrays.id.len().saturating_sub(self.index);
        (remaining, Some(remaining))
    }
}

impl<'a> ExactSizeIterator for ArrowLogRecordIter<'a> {}

/// View for a single log record.
pub struct ArrowLogRecord<'a> {
    parent: &'a ArrowLogsData<'a>,
    index: usize,
}

impl<'a> LogRecordView for ArrowLogRecord<'a> {
    type Attribute<'att> = ArrowAttribute<'att> where Self: 'att;
    type AttributeIter<'att> = ArrowLogAttributeIter<'att> where Self: 'att;
    type Body<'bod> = ArrowAnyValue<'bod> where Self: 'bod;

    fn time_unix_nano(&self) -> Option<u64> {
        self.parent
            .logs_arrays
            .time_unix_nano
            .as_ref()
            .and_then(|arr| {
                let value = arr.value(self.index);
                if value == 0 {
                    None
                } else {
                    Some(value as u64)
                }
            })
    }

    fn observed_time_unix_nano(&self) -> Option<u64> {
        self.parent
            .logs_arrays
            .observed_time_unix_nano
            .as_ref()
            .and_then(|arr| {
                let value = arr.value(self.index);
                if value == 0 {
                    None
                } else {
                    Some(value as u64)
                }
            })
    }

    fn severity_number(&self) -> Option<i32> {
        self.parent
            .logs_arrays
            .severity_number
            .as_ref()
            .and_then(|accessor| accessor.value_at(self.index))
    }

    fn severity_text(&self) -> Option<Str<'_>> {
        self.parent
            .logs_arrays
            .severity_text
            .as_ref()
            .and_then(|accessor| accessor.str_at(self.index))
            .map(|s| s.as_bytes())
    }

    fn body(&self) -> Option<Self::Body<'_>> {
        // Check if body is present via body arrays
        let body_arrays = self.parent.logs_arrays.body.as_ref()?;

        Some(ArrowAnyValue {
            anyval_arrays: &body_arrays.anyval_arrays,
            index: self.index,
        })
    }

    fn attributes(&self) -> Self::AttributeIter<'_> {
        // Get log ID for this record
        let log_id = self.parent.logs_arrays.id.value(self.index);

        ArrowLogAttributeIter {
            parent: self.parent,
            log_id,
            started: false,
        }
    }

    fn dropped_attributes_count(&self) -> u32 {
        0
    }

    fn flags(&self) -> Option<u32> {
        self.parent
            .logs_arrays
            .flags
            .as_ref()
            .map(|arr| arr.value(self.index))
    }

    fn trace_id(&self) -> Option<&TraceId> {
        self.parent
            .logs_arrays
            .trace_id
            .as_ref()
            .and_then(|accessor| {
                accessor.slice_at(self.index).and_then(|bytes| {
                    if bytes.len() == 16 && !bytes.iter().all(|&b| b == 0) {
                        bytes.try_into().ok()
                    } else {
                        None
                    }
                })
            })
    }

    fn span_id(&self) -> Option<&SpanId> {
        self.parent
            .logs_arrays
            .span_id
            .as_ref()
            .and_then(|accessor| {
                accessor.slice_at(self.index).and_then(|bytes| {
                    if bytes.len() == 8 && !bytes.iter().all(|&b| b == 0) {
                        bytes.try_into().ok()
                    } else {
                        None
                    }
                })
            })
    }

    fn event_name(&self) -> Option<Str<'_>> {
        // OTAP doesn't currently have event_name field
        None
    }
}

// ============================================================================
// Attribute Iteration
// ============================================================================

/// Iterator over log-level attributes using cursor-based sorted traversal.
///
/// Note: This iterator stores the current position and manually iterates
/// through the sorted attributes, borrowing the cursor from RefCell on each call.
pub struct ArrowLogAttributeIter<'a> {
    parent: &'a ArrowLogsData<'a>,
    log_id: u16,
    started: bool,
}

impl<'a> Iterator for ArrowLogAttributeIter<'a> {
    type Item = ArrowAttribute<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let attrs = self.parent.log_attrs_arrays.as_ref()?;

        // Borrow cursor mutably for each iteration (required due to RefCell)
        let mut cursor = self.parent.log_attrs_cursor.borrow_mut();

        // Reset cursor position on first call (but keep sorted_indices!)
        if !self.started {
            cursor.curr_index = 0;  // ✅ Just reset position, don't call reset() which clears sorted_indices!
            self.started = true;
        }

        // Manually implement ChildIndexIter logic to avoid lifetime issues
        loop {
            if cursor.curr_index >= cursor.sorted_indices.len() {
                return None;
            }

            let attr_idx = cursor.sorted_indices[cursor.curr_index];
            cursor.curr_index += 1;

            // Check if this attribute belongs to the current log
            let parent_id = attrs.parent_id.value_at(attr_idx)?;

            if parent_id == self.log_id {
                return Some(ArrowAttribute {
                    attrs,
                    index: attr_idx,
                });
            } else if parent_id > self.log_id {
                // Early termination: sorted, so no more matches
                return None;
            }
            // parent_id < self.log_id: continue to next attribute
        }
    }
}

/// View for a single attribute (key-value pair).
pub struct ArrowAttribute<'a> {
    attrs: &'a Attribute16Arrays<'a>,
    index: usize,
}

impl<'a> AttributeView for ArrowAttribute<'a> {
    type Val<'val> = ArrowAnyValue<'val> where Self: 'val;

    fn key(&self) -> Str<'_> {
        self.attrs
            .attr_key
            .str_at(self.index)
            .unwrap_or("")
            .as_bytes()
    }

    fn value(&self) -> Option<Self::Val<'_>> {
        Some(ArrowAnyValue {
            anyval_arrays: &self.attrs.anyval_arrays,
            index: self.index,
        })
    }
}

// ============================================================================
// AnyValue (Typed Values)
// ============================================================================

/// View for typed attribute values (string, int, bool, double, bytes).
pub struct ArrowAnyValue<'a> {
    anyval_arrays: &'a otel_arrow_rust::otlp::common::AnyValueArrays<'a>,
    index: usize,
}

impl<'a> AnyValueView<'a> for ArrowAnyValue<'a> {
    type KeyValue = ArrowAttribute<'a>;
    type ArrayIter<'arr> = std::iter::Empty<Self> where Self: 'arr;
    type KeyValueIter<'kv> = std::iter::Empty<Self::KeyValue> where Self: 'kv;

    fn value_type(&self) -> ValueType {
        let type_code = self.anyval_arrays.attr_type.value(self.index);

        // NOTE: Align mapping with writer enum AttributeValueType (otel-arrow-rust/src/otlp/attributes.rs):
        //   AttributeValueType::{Empty=0, Str=1, Int=2, Double=3, Bool=4, Map=5, Slice=6, Bytes=7}
        // Previous (incorrect) mapping treated 2=Bool,3=Int64,4=Double,5=Bytes which caused Int values
        // (e.g. status_code) to appear as Bool with missing payload in Bond.
        match type_code {
            0 => ValueType::Empty,
            1 => ValueType::String,
            2 => ValueType::Int64,        // Int
            3 => ValueType::Double,       // Double
            4 => ValueType::Bool,         // Bool
            5 => ValueType::KeyValueList, // Map
            6 => ValueType::Array,        // Slice (array)
            7 => ValueType::Bytes,        // Bytes
            _ => ValueType::Empty,
        }
    }

    fn as_string(&self) -> Option<Str<'_>> {
        if self.value_type() != ValueType::String {
            return None;
        }
        self.anyval_arrays
            .attr_str
            .as_ref()
            .and_then(|accessor| accessor.str_at(self.index))
            .map(|s| s.as_bytes())
    }

    fn as_bool(&self) -> Option<bool> {
        if self.value_type() != ValueType::Bool {
            return None;
        }
        self.anyval_arrays
            .attr_bool
            .as_ref()
            .map(|arr| arr.value(self.index))
    }

    fn as_int64(&self) -> Option<i64> {
        if self.value_type() != ValueType::Int64 {
            return None;
        }
        self.anyval_arrays
            .attr_int
            .as_ref()
            .and_then(|accessor| accessor.value_at(self.index))
    }

    fn as_double(&self) -> Option<f64> {
        if self.value_type() != ValueType::Double {
            return None;
        }
        self.anyval_arrays
            .attr_double
            .as_ref()
            .map(|arr| arr.value(self.index))
    }

    fn as_bytes(&self) -> Option<&[u8]> {
        if self.value_type() != ValueType::Bytes {
            return None;
        }
        self.anyval_arrays
            .attr_bytes
            .as_ref()
            .and_then(|accessor| accessor.slice_at(self.index))
    }

    fn as_array(&self) -> Option<Self::ArrayIter<'_>> {
        // OTAP doesn't currently support nested arrays in the schema we're using
        None
    }

    fn as_kvlist(&self) -> Option<Self::KeyValueIter<'_>> {
        // OTAP doesn't currently support nested kvlists in the schema we're using
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::*;
    use arrow::datatypes::{DataType, Field, Schema};
    use std::sync::Arc;

    #[test]
    fn test_arrow_logs_data_basic() {
        // Create minimal Arrow batch
        let schema = Schema::new(vec![
            Field::new("id", DataType::UInt16, false),
            Field::new("time_unix_nano", DataType::Timestamp(arrow::datatypes::TimeUnit::Nanosecond, None), true),
        ]);

        let batch = RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(UInt16Array::from(vec![1, 2, 3])),
                Arc::new(TimestampNanosecondArray::from(vec![
                    Some(1000000000),
                    Some(2000000000),
                    Some(3000000000),
                ])),
            ],
        )
        .unwrap();

        let arrow_logs = ArrowLogsData::new(&batch, None, None).unwrap();

        let mut count = 0;
        for resource_logs in arrow_logs.resources() {
            for scope_logs in resource_logs.scopes() {
                for _log in scope_logs.log_records() {
                    count += 1;
                }
            }
        }

        assert_eq!(count, 3);
    }
}
