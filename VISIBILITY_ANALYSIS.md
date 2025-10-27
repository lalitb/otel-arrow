# Visibility Analysis: What Must Remain Public?

## Question
Can the structs made public in otel-arrow-rust be reverted back to `pub(crate)` now that we have `ArrowLogsData` views?

## Answer: NO - They Must Remain Public

### Reason: Cross-Crate Usage

The types **must remain public** because they are used by **external crates**:

1. **`geneva-uploader`** in `opentelemetry-rust-contrib`
2. **`otap-df-pdata`** in `otel-arrow/otap-dataflow`

Rust's `pub(crate)` visibility only works **within the same crate**. Since these are different crates, `pub` visibility is required.

---

## Current Public API Surface

### What We Made Public (otel-arrow-rust)

```rust
// src/otlp.rs
pub mod common;
pub mod attributes;
pub mod logs;

pub use common::{
    ProtoBuffer,
    BatchSorter,           // ← Used by Geneva & ArrowLogsData
    ChildIndexIter,        // ← Used by Geneva
    SortedBatchCursor,     // ← Used by Geneva & ArrowLogsData
    AnyValueArrays,        // ← Used by ArrowLogsData
};

pub use crate::arrays::{
    NullableArrayAccessor, // ← Used by Geneva & ArrowLogsData
    ByteArrayAccessor,     // ← Type alias
    StringArrayAccessor,   // ← Type alias
    Int32ArrayAccessor,    // ← Type alias
};
```

### Who Uses What

| Type | Geneva Uploader | ArrowLogsData | Status |
|------|----------------|---------------|--------|
| `LogsArrays` | ✅ Direct use | ✅ Internal use | **Must stay public** |
| `Attribute16Arrays` | ✅ Direct use | ✅ Internal use | **Must stay public** |
| `BatchSorter` | ✅ Direct use | ✅ Internal use | **Must stay public** |
| `SortedBatchCursor` | ✅ Direct use | ✅ Internal use | **Must stay public** |
| `ChildIndexIter` | ✅ Direct use | ❌ Not used | **Must stay public** (Geneva needs it) |
| `AnyValueArrays` | ❌ Not used | ✅ Internal use | **Must stay public** (field of Attribute16Arrays) |
| `NullableArrayAccessor` | ✅ Direct use | ✅ Internal use | **Must stay public** |

---

## Two Design Paths

### Option A: Keep Current Design (Recommended)

**Public API:**
- ✅ `LogsArrays`, `Attribute16Arrays`
- ✅ `BatchSorter`, `SortedBatchCursor`, `ChildIndexIter`
- ✅ `AnyValueArrays`
- ✅ `NullableArrayAccessor` and type aliases
- ✅ `ArrowLogsData` (in pdata)

**Pros:**
- ✅ Maximum flexibility for external exporters
- ✅ Supports both direct access AND view-based access
- ✅ No breaking changes to Geneva
- ✅ Matches RFC design
- ✅ Allows performance-critical code to use low-level APIs

**Cons:**
- ❌ Larger public API surface (more maintenance burden)
- ❌ Two ways to do the same thing (potential confusion)

**Geneva Usage:**
```rust
// Can continue using direct access
let logs_arrays = LogsArrays::try_from(logs_batch)?;
let mut cursor = SortedBatchCursor::new();
self.batch_sorter.init_cursor_for_u16_id_column(&attrs.parent_id, &mut cursor);
```

OR

```rust
// Can migrate to cleaner view API
let arrow_logs = ArrowLogsData::new(logs_batch, log_attrs_batch, resource_attrs_batch)?;
for log in scope_logs.log_records() { /* ... */ }
```

---

### Option B: Only Export ArrowLogsData (Alternative)

**Public API:**
- ✅ `ArrowLogsData` (in pdata)
- ❌ Revert cursors to `pub(crate)` in otel-arrow-rust
- ❌ Revert `LogsArrays`, `Attribute16Arrays` to `pub(crate)`

**Pros:**
- ✅ Smaller, cleaner public API
- ✅ Forces best practices (use views)
- ✅ Less maintenance burden

**Cons:**
- ❌ **BREAKING CHANGE** for Geneva uploader
- ❌ Requires immediate migration of Geneva code
- ❌ Less flexibility for performance-critical use cases
- ❌ Doesn't match RFC (which proposed making them public)
- ❌ **Won't work** - pdata needs these types internally!

**Why This Won't Work:**

Even if we hide the types from Geneva, **ArrowLogsData itself** needs them:

```rust
// ArrowLogsData internally uses these types
pub struct ArrowLogsData<'a> {
    logs_arrays: LogsArrays<'a>,           // ← Needs LogsArrays
    log_attrs_arrays: Option<Attribute16Arrays<'a>>, // ← Needs Attribute16Arrays
    batch_sorter: BatchSorter,             // ← Needs BatchSorter
    log_attrs_cursor: RefCell<SortedBatchCursor>, // ← Needs SortedBatchCursor
}
```

Since ArrowLogsData is in a **different crate** (pdata), these types **must be public** in otel-arrow-rust for pdata to use them!

---

## Recommendation

**✅ Keep the current design (Option A)** as specified in the RFC:

### Reasons:

1. **Technical Necessity**: pdata crate needs these types to be public to implement ArrowLogsData
2. **Follows RFC**: The RFC was specifically about making these utilities public
3. **Backward Compatible**: No breaking changes to existing Geneva code
4. **Flexibility**: Allows both direct access (power users) and view-based access (convenience)
5. **Ecosystem Pattern**: Similar to how other Rust crates expose building blocks:
   - **Tokio** exposes `AsyncRead`/`AsyncWrite` traits for custom transports
   - **Arrow** exposes array types and accessors
   - **otel-arrow** (now) exposes traversal utilities for custom exporters

### Semantic Versioning Commitment

Since these are now public:
- **Minor version changes**: Can add new methods/fields
- **Major version changes**: Required for removing public items or changing signatures
- **Documentation**: Should be improved with examples and performance characteristics

---

## Migration Path (Optional, Future)

If we decide to **eventually** make the API cleaner, we could:

1. **Phase 1** (Current): Keep both APIs available
2. **Phase 2** (Future): Mark direct access as `#[deprecated]` with message pointing to views
3. **Phase 3** (Next major version): Remove direct access

But this is **optional** and **not recommended** because:
- Direct access is useful for performance-critical code
- Standard library pattern is to expose building blocks
- Current API is already well-designed

---

## Summary

| Question | Answer | Reason |
|----------|--------|--------|
| Can we revert to `pub(crate)`? | ❌ **NO** | pdata crate needs them |
| Should we revert to `pub(crate)`? | ❌ **NO** | RFC specifies public, provides flexibility |
| What about AnyValueArrays? | ✅ **Must be public** | Field of Attribute16Arrays |
| What about ChildIndexIter? | ✅ **Must be public** | Geneva uses it directly |
| Is this the right design? | ✅ **YES** | Matches Rust ecosystem patterns |

**Final Recommendation: Keep everything public as currently implemented. This is the correct, idiomatic design.**
