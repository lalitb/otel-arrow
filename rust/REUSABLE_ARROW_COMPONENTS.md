# Reusable OTAP Arrow Components - Analysis & Recommendations

## Executive Summary

**Goal**: Extract reusable components from `arrow_log_encoder.rs` into `otel-arrow-rust` for use by multiple exporters.

**Current State**:
- ✅ `otel-arrow-rust` has good foundation (`LogsArrays`, `AttributeArrays`, etc.)
- ❌ Geneva encoder duplicates some functionality
- 🎯 Opportunity to create exporter-agnostic iteration utilities

---

## Current Architecture

### In `otel-arrow-rust/src/otlp/logs.rs`

**Already Exists** ✅:
```rust
struct LogsArrays<'a> {
    id: &'a UInt16Array,
    time_unix_nano: Option<&'a TimestampNanosecondArray>,
    observed_time_unix_nano: Option<&'a TimestampNanosecondArray>,
    trace_id: Option<ByteArrayAccessor<'a>>,
    span_id: Option<ByteArrayAccessor<'a>>,
    severity_number: Option<Int32ArrayAccessor<'a>>,
    severity_text: Option<StringArrayAccessor<'a>>,
    body: Option<LogBodyArrays<'a>>,
    // ... etc
}
```

**Purpose**: Zero-copy accessors for log record columns

### In `otel-arrow-rust/src/otlp/attributes.rs`

**Already Exists** ✅:
```rust
pub(crate) struct AttributeArrays<'a, T: ArrowPrimitiveType> {
    pub parent_id: MaybeDictArrayAccessor<'a, PrimitiveArray<T>>,
    pub attr_key: MaybeDictArrayAccessor<'a, StringArray>,
    pub anyval_arrays: AnyValueArrays<'a>,
}
```

**Purpose**: Attribute column accessors with parent-child relationship

### In `geneva-uploader/arrow_log_encoder.rs`

**Custom Implementation** ❌:
```rust
struct ArrowColumns<'a> {
    id: Option<&'a UInt16Array>,
    time_unix_nano: Option<&'a TimestampNanosecondArray>,
    severity_number: Option<&'a dyn Array>,
    severity_text: Option<&'a dyn Array>,
    body: Option<&'a dyn Array>,
    // ... duplicates LogsArrays!
}

// Custom attribute map building
fn build_log_attrs_map(attrs_batch: &RecordBatch)
    -> Result<HashMap<u16, Vec<(String, u8, usize)>>, String>
{
    // Manually iterates over parent_id, key, type, str columns
    // Allocates strings (performance issue!)
}
```

---

## Problem Analysis

### 🔴 **Issue 1: Code Duplication**

**Geneva's `ArrowColumns`** vs **otel-arrow's `LogsArrays`**:
- Same purpose: Zero-copy column accessors
- Different implementations
- Geneva version is simpler but less robust

### 🔴 **Issue 2: Attribute Iteration**

Geneva encoder manually builds attribute maps:
```rust
// Lines 469-508: build_log_attrs_map
// Lines 511-575: build_resource_attrs_map
// Lines 577-670: extract_attribute_value
```

**Problems**:
1. Allocates `String` for every key/value (performance)
2. Not reusable by other exporters
3. Duplicates logic from `otel-arrow-rust`

### 🔴 **Issue 3: No Iterator Pattern**

Geneva encoder uses index-based loops:
```rust
for row_idx in 0..num_rows {
    // Get data by index
    let timestamp = arrow_columns.get_timestamp(row_idx);
    // ...
}
```

**Missing**: Rust-idiomatic iterators for row-by-row processing

---

## Recommendations

### 📦 **Component 1: Log Record Iterator** (HIGH PRIORITY)

**Add to**: `otel-arrow-rust/src/otlp/logs.rs`

```rust
/// Iterator over log records in Arrow format (zero-copy)
pub struct LogRecordIter<'a> {
    logs_arrays: LogsArrays<'a>,
    log_attrs: Option<Attribute16Arrays<'a>>,
    current_row: usize,
    num_rows: usize,
}

impl<'a> LogRecordIter<'a> {
    pub fn new(
        logs_batch: &'a RecordBatch,
        log_attrs_batch: Option<&'a RecordBatch>,
    ) -> Result<Self> {
        let logs_arrays = LogsArrays::try_from(logs_batch)?;
        let log_attrs = log_attrs_batch
            .map(Attribute16Arrays::try_from)
            .transpose()?;

        Ok(Self {
            logs_arrays,
            log_attrs,
            current_row: 0,
            num_rows: logs_batch.num_rows(),
        })
    }
}

/// Represents a single log record (borrowed data)
pub struct LogRecordView<'a> {
    pub row_idx: usize,
    pub log_id: Option<u16>,
    pub timestamp: Option<u64>,
    pub observed_timestamp: Option<u64>,
    pub severity_number: Option<i32>,
    pub severity_text: Option<&'a str>,
    pub body: Option<&'a str>,  // Extracted from AnyValue
    pub trace_id: Option<&'a [u8]>,
    pub span_id: Option<&'a [u8]>,
    pub flags: Option<u32>,
    // Attributes accessed via separate iterator
}

impl<'a> Iterator for LogRecordIter<'a> {
    type Item = Result<LogRecordView<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current_row >= self.num_rows {
            return None;
        }

        let row_idx = self.current_row;
        self.current_row += 1;

        Some(self.extract_row(row_idx))
    }
}

impl<'a> LogRecordIter<'a> {
    fn extract_row(&self, row_idx: usize) -> Result<LogRecordView<'a>> {
        // Zero-copy extraction from Arrow arrays
        Ok(LogRecordView {
            row_idx,
            log_id: self.logs_arrays.id.value_at(row_idx),
            timestamp: self.logs_arrays.time_unix_nano
                .and_then(|arr| arr.value_at(row_idx)),
            severity_number: self.logs_arrays.severity_number
                .and_then(|arr| arr.value_at(row_idx)),
            severity_text: self.logs_arrays.severity_text
                .and_then(|arr| arr.str_at(row_idx)),
            body: self.extract_body(row_idx)?,
            // ... etc
        })
    }
}
```

**Benefits**:
- ✅ Zero-copy (borrows from Arrow)
- ✅ Rust-idiomatic iterator pattern
- ✅ Reusable across all exporters
- ✅ Type-safe access to log fields

---

### 📦 **Component 2: Attribute Iterator** (HIGH PRIORITY)

**Add to**: `otel-arrow-rust/src/otlp/attributes.rs`

```rust
/// Iterator over attributes for a specific parent entity (log, span, etc.)
pub struct AttributeIter<'a, T: ArrowPrimitiveType> {
    attr_arrays: &'a AttributeArrays<'a, T>,
    parent_id: T::Native,
    indices: Vec<usize>,  // Pre-computed indices for this parent
    current: usize,
}

impl<'a, T: ArrowPrimitiveType> AttributeIter<'a, T> {
    /// Create iterator for attributes of a specific parent
    pub fn for_parent(
        attr_arrays: &'a AttributeArrays<'a, T>,
        parent_id: T::Native,
    ) -> Self {
        // Build index list (could be cached in a map)
        let indices = (0..attr_arrays.parent_id.len())
            .filter(|&i| {
                attr_arrays.parent_id.value_at(i) == Some(parent_id)
            })
            .collect();

        Self {
            attr_arrays,
            parent_id,
            indices,
            current: 0,
        }
    }
}

/// View of a single attribute (zero-copy)
pub struct AttributeView<'a> {
    pub key: &'a str,
    pub value_type: AttributeValueType,
    pub value_index: usize,  // Index into value columns
}

impl<'a, T: ArrowPrimitiveType> Iterator for AttributeIter<'a, T> {
    type Item = Result<AttributeView<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current >= self.indices.len() {
            return None;
        }

        let idx = self.indices[self.current];
        self.current += 1;

        Some(self.extract_attribute(idx))
    }
}

impl<'a, T: ArrowPrimitiveType> AttributeIter<'a, T> {
    fn extract_attribute(&self, idx: usize) -> Result<AttributeView<'a>> {
        let key = self.attr_arrays.attr_key.str_at(idx)
            .ok_or("Missing attribute key")?;

        let value_type = self.attr_arrays.anyval_arrays.attr_type.value_at(idx)
            .and_then(|t| AttributeValueType::try_from(t).ok())
            .ok_or("Invalid attribute type")?;

        Ok(AttributeView {
            key,
            value_type,
            value_index: idx,
        })
    }

    /// Get string value for this attribute
    pub fn get_string_value(&self, view: &AttributeView) -> Option<&'a str> {
        self.attr_arrays.anyval_arrays.attr_str
            .and_then(|arr| arr.str_at(view.value_index))
    }

    /// Get int value for this attribute
    pub fn get_int_value(&self, view: &AttributeView) -> Option<i64> {
        self.attr_arrays.anyval_arrays.attr_int
            .and_then(|arr| arr.value_at(view.value_index))
    }

    // ... etc for other types
}
```

**Benefits**:
- ✅ Zero-copy (borrows strings from Arrow dictionaries)
- ✅ No String allocations!
- ✅ Type-safe value access
- ✅ Reusable pattern

---

### 📦 **Component 3: Attribute Index Map** (MEDIUM PRIORITY)

**Add to**: `otel-arrow-rust/src/otlp/attributes.rs`

```rust
/// Pre-computed map of parent_id -> attribute indices (avoids repeated lookups)
pub struct AttributeIndexMap<T: ArrowPrimitiveType> {
    indices: HashMap<T::Native, Vec<usize>>,
}

impl<T: ArrowPrimitiveType> AttributeIndexMap<T> {
    /// Build index map from attribute batch (do once per batch)
    pub fn build(attr_arrays: &AttributeArrays<'_, T>) -> Self {
        let mut indices: HashMap<T::Native, Vec<usize>> = HashMap::new();

        for idx in 0..attr_arrays.parent_id.len() {
            if let Some(parent_id) = attr_arrays.parent_id.value_at(idx) {
                indices.entry(parent_id)
                    .or_insert_with(Vec::new)
                    .push(idx);
            }
        }

        Self { indices }
    }

    /// Get attribute indices for a specific parent (zero-copy)
    pub fn get_indices(&self, parent_id: T::Native) -> Option<&[usize]> {
        self.indices.get(&parent_id).map(|v| v.as_slice())
    }

    /// Check if parent has attributes
    pub fn has_attributes(&self, parent_id: T::Native) -> bool {
        self.indices.get(&parent_id)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }
}
```

**Benefits**:
- ✅ Build once, use many times
- ✅ O(1) lookup instead of O(n) scan
- ✅ No string allocations
- ✅ Cache-friendly

---

## Proposed Geneva Encoder Refactoring

### Before (Current)
```rust
// Custom ArrowColumns struct
let arrow_columns = ArrowColumns::extract(logs_batch)?;

// Custom attribute map (allocates strings!)
let log_attrs_map = Self::build_log_attrs_map(attrs_batch)?;

// Manual iteration
for row_idx in 0..num_rows {
    let timestamp = arrow_columns.get_timestamp(row_idx);
    let log_id = arrow_columns.get_log_id(row_idx);

    // Manual attribute lookup
    if let Some(attrs) = log_attrs_map.get(&log_id) {
        for (key, type_id, idx) in attrs {
            // Use key (String - allocated!)
        }
    }
}
```

### After (Using otel-arrow components)
```rust
use otel_arrow_rust::otlp::logs::LogRecordIter;
use otel_arrow_rust::otlp::attributes::{AttributeIter, AttributeIndexMap};

// Use otel-arrow's LogsArrays
let log_iter = LogRecordIter::new(logs_batch, log_attrs_batch)?;

// Build attribute index map once
let attr_index_map = log_attrs_batch
    .map(|batch| AttributeIndexMap::build(&Attribute16Arrays::try_from(batch)?))
    .transpose()?;

// Iterate using iterator pattern
for log_record in log_iter {
    let log = log_record?;

    // Use log fields (zero-copy)
    write_timestamp(&log.timestamp);
    write_severity(&log.severity_number);
    write_body(&log.body);

    // Iterate attributes (zero-copy!)
    if let Some(ref index_map) = attr_index_map {
        if let Some(log_id) = log.log_id {
            let attr_iter = AttributeIter::for_parent_indices(
                &attr_arrays,
                index_map.get_indices(log_id)
            );

            for attr in attr_iter {
                let attr = attr?;
                // Use attr.key (borrowed &str - no allocation!)
                write_attribute(attr.key, attr.get_value());
            }
        }
    }
}
```

**Benefits**:
- ✅ No code duplication
- ✅ Zero string allocations
- ✅ Iterator-based (Rust idiomatic)
- ✅ Reusable by other exporters
- ✅ Type-safe

---

## Implementation Plan

### Phase 1: Add Core Iterators (1-2 days)
1. Add `LogRecordIter` to `otel-arrow-rust/src/otlp/logs.rs`
2. Add `AttributeIter` to `otel-arrow-rust/src/otlp/attributes.rs`
3. Add `AttributeIndexMap` to `otel-arrow-rust/src/otlp/attributes.rs`
4. Write comprehensive tests

### Phase 2: Refactor Geneva Encoder (1 day)
5. Remove custom `ArrowColumns` struct
6. Use `LogRecordIter` instead of manual loops
7. Use `AttributeIter` instead of `build_log_attrs_map`
8. Remove all `.to_string()` allocations

### Phase 3: Documentation & Examples (0.5 day)
9. Document the new iterator APIs
10. Add usage examples
11. Update existing code to use new patterns

**Total Effort**: ~3-4 days

---

## Impact Analysis

### Performance
- **Before**: ~200K string allocations/sec (at 10K logs/sec)
- **After**: ~0 string allocations/sec (zero-copy iterators)
- **Improvement**: 99.9% reduction

### Code Reusability
- ✅ Other exporters can use same components
- ✅ Prometheus exporter
- ✅ Custom exporters
- ✅ Data transformation pipelines

### Maintainability
- ✅ Less code duplication
- ✅ Single source of truth
- ✅ Easier to optimize (optimize once, everyone benefits)
- ✅ Better tested (shared code)

---

## Alternative Approaches

### Option A: Keep Current Geneva Code (Not Recommended)
**Pros**: No work needed
**Cons**: Duplicated code, performance issues, not reusable

### Option B: Extract Minimal Components (Medium Effort)
**Pros**: Quick win
**Cons**: Still some duplication
**Scope**: Just `AttributeIndexMap`

### Option C: Full Iterator Pattern (Recommended)
**Pros**: Best performance, most reusable, Rust-idiomatic
**Cons**: Moderate effort
**Scope**: All components described above

---

## API Design Principles

1. **Zero-Copy**: All iterators borrow from Arrow, no allocations
2. **Type-Safe**: Use Rust's type system, not runtime checks
3. **Ergonomic**: Rust-idiomatic iterators and combinators
4. **Composable**: Mix and match components
5. **Generic**: Work with UInt16 or UInt32 parent IDs
6. **Documented**: Clear docs and examples

---

## Conclusion

**Recommendation**: Implement **Option C** (Full Iterator Pattern)

**Benefits**:
- 🚀 99.9% reduction in allocations
- ♻️ Reusable across all exporters
- 🎯 Single source of truth
- 📚 Well-documented API
- ✅ Rust-idiomatic design

**Effort**: 3-4 days
**Impact**: HIGH (performance + reusability)

The components will live in `otel-arrow-rust` and be available to:
- Geneva exporter
- Prometheus exporter
- Any future Arrow-based exporters
- Data processing pipelines
- Custom telemetry backends

This creates a solid foundation for the OTAP ecosystem! 🎉
