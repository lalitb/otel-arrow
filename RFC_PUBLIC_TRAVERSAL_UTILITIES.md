# RFC: Export Arrow Traversal Utilities for External OTAP Exporters

## Status
Proposed

## Summary
Make key Arrow traversal utilities (`LogsArrays`, `Attribute16Arrays`, `BatchSorter`, `ChildIndexIter`, `SortedBatchCursor`) public to enable external crates to build custom OTAP exporters with zero-copy semantics.

## Problem Statement

Currently, `otel-arrow-rust` provides excellent internal utilities for traversing OTAP Arrow record batches with zero-copy semantics. However, these utilities are marked `pub(crate)`, making them inaccessible to external exporters.

This creates a dilemma for downstream telemetry backends that want to build OTAP-native exporters:

1. **Duplicate Code**: External exporters must reimplement ~450+ lines of Arrow traversal logic, violating DRY principles
2. **Inconsistency Risk**: Different implementations may handle edge cases differently or miss optimizations
3. **Maintenance Burden**: Each exporter must maintain their own copy of sorting, cursor management, and attribute iteration
4. **Lost Zero-Copy Benefits**: Without access to the dictionary-aware accessors, external exporters may fall back to allocation-heavy approaches

## Motivation: Geneva Uploader as Reference Implementation

The Geneva uploader provides a concrete use case that illustrates the broader ecosystem need:

### Geneva Uploader Architecture
```
┌─────────────────────────────────────────────────────────────┐
│  opentelemetry-rust-contrib/geneva-uploader                 │
│  (Reusable infrastructure - vendor SDK)                     │
├─────────────────────────────────────────────────────────────┤
│  • OTLP encoder (protobuf → Bond)                          │
│  • OTAP encoder (Arrow → Bond)  ← needs otel-arrow utils   │
│  • Auth (MSI, WorkloadIdentity, Certificate)               │
│  • Uploader (config service + ingestion service)           │
│  • Client (unified interface)                              │
└─────────────────────────────────────────────────────────────┘
                           ▲
          ┌────────────────┼────────────────┐
          │                │                │
    ┌─────┴─────┐   ┌─────┴─────┐   ┌─────┴─────┐
    │  Go FFI   │   │   Rust    │   │ OTel SDK  │
    │ Collector │   │ Collector │   │  Exporter │
    └───────────┘   └───────────┘   └───────────┘
```

**Why geneva-uploader lives in opentelemetry-rust-contrib:**
- It's **reusable vendor infrastructure** (analogous to Jaeger/Zipkin exporters)
- Used by multiple consumers: Go collector (via FFI), Rust collector (native), OTel SDK exporter
- Provides complete solution: encoding + auth + upload + retry logic
- Separates telemetry-agnostic pipeline from vendor-specific SDK

**Why it needs otel-arrow utilities:**
- Must support **both OTLP (protobuf) and OTAP (Arrow)** formats
- OTAP path requires zero-copy iteration of Arrow batches before Bond encoding
- Should not duplicate 450+ lines of traversal code from otel-arrow

### Why This Pattern Benefits the Broader Ecosystem

Geneva is not unique. Any backend wanting to build an OTAP-native exporter faces similar needs:
- **Datadog**: Arrow → Datadog Sketch format
- **Splunk**: Arrow → HEC JSON batches
- **ClickHouse**: Arrow → Native ClickHouse binary protocol
- **Custom backends**: Arrow → proprietary wire formats

All require:
1. Efficient parent-child attribute iteration
2. Zero-copy string access from Arrow dictionaries
3. Sorted traversal for matching log records to attributes

## Proposed Solution

### Make These Types Public

#### Core Traversal Utilities
```rust
// src/otlp/common.rs
pub struct BatchSorter { /* ... */ }
pub struct SortedBatchCursor { /* ... */ }
pub struct ChildIndexIter<'a, T: ArrowPrimitiveType> { /* ... */ }
```

#### Zero-Copy Accessor Types
```rust
// src/otlp/logs.rs
pub struct LogsArrays<'a> { /* ... */ }
pub struct LogBodyArrays<'a> { /* ... */ }

// src/otlp/attributes.rs
pub type Attribute16Arrays<'a> = AttributeArrays<'a, UInt16Type>;
pub struct AttributeArrays<'a, T: ArrowPrimitiveType> { /* ... */ }

// src/otlp/common.rs
pub struct AnyValueArrays<'a> { /* ... */ }

// src/arrays.rs
pub trait NullableArrayAccessor { /* ... */ }
pub type StringArrayAccessor<'a> = MaybeDictArrayAccessor<'a, StringArray>;
pub type ByteArrayAccessor<'a> = /* ... */;
```

### Documentation and Examples

Provide comprehensive documentation showing:

1. **Basic usage pattern:**
```rust
use otel_arrow_rust::otlp::{BatchSorter, ChildIndexIter, SortedBatchCursor};
use otel_arrow_rust::otlp::logs::LogsArrays;
use otel_arrow_rust::otlp::attributes::Attribute16Arrays;

// Parse Arrow batches with zero-copy accessors
let logs_arrays = LogsArrays::try_from(logs_batch)?;
let attrs_arrays = Attribute16Arrays::try_from(log_attrs_batch)?;

// Initialize cursor for sorted traversal
let mut batch_sorter = BatchSorter::new();
let mut attrs_cursor = SortedBatchCursor::new();
batch_sorter.init_cursor_for_u16_id_column(&attrs_arrays.parent_id, &mut attrs_cursor);

// Iterate log records and their attributes
for log_idx in 0..logs_arrays.id.len() {
    let log_id = logs_arrays.id.value(log_idx);

    // Iterate attributes for this log (with early termination optimization)
    for attr_idx in ChildIndexIter::new(log_id, &attrs_arrays.parent_id, &mut attrs_cursor) {
        // Zero-copy string access from dictionary
        if let Some(key) = attrs_arrays.attr_key.str_at(attr_idx) {
            // key is &str - no allocation!
            encode_attribute(key, &attrs_arrays.anyval_arrays, attr_idx);
        }
    }
}
```

2. **Performance characteristics:**
   - Sorting: O(n log n) once per batch
   - Cursor iteration: O(1) per attribute access with early termination
   - String access: Zero-copy from Arrow dictionaries

3. **Reference implementation:** Link to geneva-uploader's usage

## Benefits

### 1. **Enables Ecosystem Growth**
External backends can build OTAP-native exporters without reimplementing core traversal logic, aligning with OpenTelemetry's vendor-neutral philosophy.

### 2. **DRY Principle**
Eliminates code duplication between otel-arrow and external exporters. Single source of truth for Arrow traversal patterns.

### 3. **Consistency and Correctness**
All exporters use the same battle-tested cursor logic, reducing bugs and edge case handling differences.

### 4. **Aligns with OTAP Phase 2 Goals**
Phase 2 aims to enable widespread OTAP adoption. Making traversal utilities public directly supports this by lowering the barrier for backend integrations.

### 5. **Standard Library Pattern**
Mirrors how other Rust ecosystem crates expose building blocks:
- **Tokio**: Exposes `AsyncRead`/`AsyncWrite` traits for custom transports
- **Serde**: Exposes `Serializer`/`Deserializer` for custom formats
- **Arrow**: Exposes `RecordBatch`, array types, and accessors for custom processing
- **otel-arrow** (this proposal): Exposes traversal utilities for custom OTAP exporters

### 6. **Zero-Copy Semantics**
External exporters gain access to dictionary-aware string accessors, enabling true zero-copy processing.

## Alternatives Considered

### Alternative 1: Move Exporters to otel-arrow Repository
**Rejected because:**
- Violates separation of concerns (protocol library vs. vendor SDKs)
- Creates tight coupling between protocol evolution and vendor integrations
- Requires otel-arrow to take on maintenance burden for all vendor backends
- Doesn't match ecosystem patterns (Serde doesn't contain all format implementations)

### Alternative 2: Duplicate Code in Each Exporter
**Rejected because:**
- Violates DRY principle
- Creates ~450 lines of maintenance burden per exporter
- Risk of inconsistent behavior across implementations
- Misses optimization opportunities (dictionary encoding, cursor reuse)

### Alternative 3: Callback-Based API
```rust
pub fn process_logs_with_attrs<F>(batch: &RecordBatch, callback: F)
where F: Fn(LogRecord, Vec<Attribute>)
```
**Rejected because:**
- Less flexible than direct access to arrays
- Forces allocation of intermediate `Vec<Attribute>`
- Doesn't support custom iteration patterns
- More restrictive API surface that may not fit all backends

### Alternative 4: Create Separate `otel-arrow-utils` Crate
**Rejected because:**
- Adds organizational complexity
- These utilities are core to the Arrow format processing
- Splitting would complicate dependency management
- Standard pattern is to expose building blocks from main crate

## Implementation Impact

### API Stability Considerations

**Question for maintainers:** What stability guarantees do you want to provide?

**Options:**
1. **Stable Public API** (recommended)
   - Types are already well-designed for internal use
   - Document as stable public API
   - Use semantic versioning for breaking changes
   - Pros: Gives external users confidence, aligns with ecosystem expectations
   - Cons: Requires more careful evolution

2. **Experimental Public API**
   - Mark types with `#[doc(hidden)]` or behind feature flag
   - Explicitly document as unstable
   - Allows more rapid iteration
   - Pros: More flexibility for otel-arrow development
   - Cons: Limits adoption, requires frequent updates in external crates

3. **Semver-Exempt Internals**
   - Expose via `pub use` but document as internal
   - Breaking changes allowed in minor versions
   - Pros: Maximum flexibility
   - Cons: Violates Rust ecosystem norms, may frustrate users

**Recommendation:** Start with stable public API. These types are mature, well-tested through internal use, and unlikely to need frequent breaking changes.

### Maintenance Burden

**Minimal additional burden:**
- Types are already well-designed and tested
- Documentation is the main addition
- Public API promotes more careful evolution (actually reduces churn)
- External usage provides additional testing coverage

### Backward Compatibility

**No breaking changes:**
- Only changes visibility modifiers from `pub(crate)` to `pub`
- Adds documentation
- No changes to behavior or signatures
- Fully backward compatible with existing code

## Questions for Maintainers

1. **Alignment with Roadmap:** Does this align with otel-arrow's vision for Phase 2 and enabling broader OTAP adoption?

2. **Stability Guarantees:** What level of API stability do you want to commit to? (Stable, experimental, or semver-exempt?)

3. **Documentation Standards:** What level of documentation do you require for public APIs? (Examples, performance characteristics, safety invariants?)

4. **Feature Gating:** Should these utilities be behind an optional feature flag (e.g., `exporter-utils`) or always available?

5. **Future Evolution:** Are there upcoming changes to these types that would make public exposure problematic?

6. **Governance:** Should there be a formal process for external exporters to provide feedback on these APIs?

## Success Metrics

1. **Code Reduction:** Geneva uploader successfully deletes ~450 lines of duplicate traversal code
2. **Ecosystem Growth:** 2+ external exporters adopt the public utilities within 6 months
3. **Zero Breaking Changes:** No unplanned breaking changes to public API in first year
4. **Performance Maintained:** External exporters achieve same zero-copy performance as internal encoders
5. **Documentation Quality:** External users can implement basic exporter without asking for help

## Timeline

1. **Phase 1 (Week 1):** Review and approval of RFC by otel-arrow maintainers
2. **Phase 2 (Week 2):** Add documentation and make types public
3. **Phase 3 (Week 3):** Geneva uploader migrates to public API (validation)
4. **Phase 4 (Week 4):** Publish new version, announce to community
5. **Phase 5 (Ongoing):** Gather feedback from external adopters

## References

- Geneva uploader implementation: `opentelemetry-rust-contrib/opentelemetry-exporter-geneva/geneva-uploader`
- OTAP Phase 2 design goals: [link to design doc if available]
- Similar patterns in Rust ecosystem:
  - Tokio's `AsyncRead`/`AsyncWrite`: https://docs.rs/tokio/latest/tokio/io/
  - Serde's `Serializer` traits: https://docs.rs/serde/latest/serde/ser/
  - Arrow's public array types: https://docs.rs/arrow/latest/arrow/

## Author

@lalitb (with implementation support from external contributors)

## Discussion

Please share your thoughts on:
- Alignment with otel-arrow's vision and roadmap
- API stability preferences
- Documentation requirements
- Alternative approaches not covered here
- Timeline and prioritization
