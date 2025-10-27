# Decision: Should Geneva Migrate to ArrowLogsData Views?

## Current Situation

**Geneva has TWO options:**

### Option A: Keep Current Implementation (Direct Access)
```rust
// Current: geneva-uploader/src/payload_encoder/arrow_log_encoder.rs
use otel_arrow_rust::otlp::{LogsArrays, Attribute16Arrays, BatchSorter, ChildIndexIter};

let logs_arrays = LogsArrays::try_from(logs_batch)?;
let mut cursor = SortedBatchCursor::new();
for attr_idx in ChildIndexIter::new(log_id, &attrs.parent_id, &mut cursor) { /* ... */ }
```

### Option B: Migrate to ArrowLogsData Views
```rust
// New: Use view-based API
use otap_df_pdata::views::otap::arrow::logs::ArrowLogsData;

let arrow_logs = ArrowLogsData::new(logs_batch, log_attrs_batch, resource_attrs_batch)?;
for log in scope_logs.log_records() {
    for attr in log.attributes() { /* Cursors hidden! */ }
}
```

---

## Analysis

### If Geneva Migrates to Views

**What Geneva needs:**
- ✅ `ArrowLogsData` (from pdata)
- ✅ View traits: `LogsDataView`, `ResourceLogsView`, etc. (from pdata)
- ❌ NO direct import of LogsArrays, cursors, etc.!

**What otel-arrow-rust must export:**
- ✅ Still need LogsArrays, Attribute16Arrays, cursors **to be public**
- ❌ **But NOT for Geneva** - for ArrowLogsData implementation in pdata!

**Key Point:**
```rust
// pdata is a DIFFERENT CRATE from otel-arrow-rust
// So even if Geneva doesn't use them, they must be public for pdata!

// pdata/src/views/otap/arrow/logs.rs
use otel_arrow_rust::otlp::logs::LogsArrays;  // ← Cross-crate import requires pub!
```

### Visibility Impact

| Scenario | LogsArrays Visibility | Why |
|----------|----------------------|-----|
| Geneva uses direct access | `pub` | Geneva imports it |
| Geneva uses views | `pub` | **pdata still imports it!** |
| ArrowLogsData moved to otel-arrow | `pub(crate)` | Same crate, but creates circular dep |

---

## The Answer

**Even if Geneva migrates to using ArrowLogsData views:**

✅ **Types STILL must be public** because:
1. ArrowLogsData is in the **pdata crate** (different crate)
2. pdata needs to import LogsArrays, Attribute16Arrays, etc.
3. Cross-crate imports require `pub` visibility

❌ **We CANNOT hide them** unless:
- We move ArrowLogsData into otel-arrow-rust (creates circular dependency)
- We duplicate LogsArrays code in pdata (violates DRY)

---

## Recommended Path

### Short Term: Keep Both Options Available

**Option 1: Direct Access (Power Users)**
```rust
// For performance-critical code that needs fine control
use otel_arrow_rust::otlp::{LogsArrays, Attribute16Arrays, BatchSorter};
let logs_arrays = LogsArrays::try_from(batch)?;
// Manual cursor management for maximum control
```

**Option 2: View-Based Access (Convenience)**
```rust
// For cleaner code with hidden complexity
use otap_df_pdata::views::otap::arrow::logs::ArrowLogsData;
let arrow_logs = ArrowLogsData::new(batch, attrs, res_attrs)?;
for log in scope_logs.log_records() { /* Clean iteration */ }
```

### Long Term: Migrate Geneva (Optional)

**If we want Geneva to use the cleaner API:**

```diff
// geneva-uploader/src/payload_encoder/arrow_log_encoder.rs

- use otel_arrow_rust::otlp::{LogsArrays, Attribute16Arrays, BatchSorter, ChildIndexIter};
+ use otap_df_pdata::views::otap::arrow::logs::ArrowLogsData;
+ use otap_df_pdata::views::logs::{LogsDataView, LogRecordView};

  pub fn encode_arrow_batch(&self, ...) -> Result<Vec<EncodedBatch>> {
-     let logs_arrays = LogsArrays::try_from(logs_batch)?;
-     let mut cursor = SortedBatchCursor::new();
-     self.batch_sorter.init_cursor_for_u16_id_column(...);

+     let arrow_logs = ArrowLogsData::new(logs_batch, log_attrs_batch, resource_attrs_batch)?;
+     for resource_logs in arrow_logs.resources() {
+         for scope_logs in resource_logs.scopes() {
+             for log in scope_logs.log_records() {
+                 // Cleaner iteration!
+                 for attr in log.attributes() { /* ... */ }
+             }
+         }
+     }
  }
```

**Benefits:**
- ✅ Cleaner Geneva code (no cursor management)
- ✅ Geneva no longer directly imports low-level types
- ✅ Consistent with view pattern used elsewhere

**But still:**
- ❌ Low-level types STILL must be `pub` (for pdata)
- ❌ No reduction in public API surface

---

## Conclusion

### Q: If Geneva uses views, can we hide LogsArrays/cursors?
**A: NO** - They must remain public for pdata to use them.

### Q: So what's the point of migrating Geneva?
**A: Cleaner code** - But it doesn't reduce the public API surface.

### Q: What should we do?
**A: Three valid approaches:**

1. **Keep both** - Geneva can choose direct or view-based access ✅ **Recommended**
2. **Migrate Geneva** - Use views for cleaner code (still need public API)
3. **Move ArrowLogsData to otel-arrow** - Reduces public API but creates circular dep ❌

### Final Recommendation

✅ **Keep current design:**
- Types remain public (as in RFC)
- Geneva can continue using direct access (works today)
- Geneva can optionally migrate to views (cleaner code)
- External exporters can choose which API fits their needs

**The public API is correct and necessary regardless of which approach Geneva uses.**
