# ArrowLogEncoder - Cloning and Allocation Analysis

**File**: `geneva-uploader/src/payload_encoder/arrow_log_encoder.rs`

**Status**: ✅ This is the CORRECT encoder being used (not the deprecated OtapLogEncoder)

---

## Critical Issues Found (Hot Path)

### 🔴 **ISSUE 1: Attribute Key Cloning in Schema Generation**
**Location**: Line 325
**Severity**: HIGH (happens per log record)

```rust
for (key, _, _) in attrs {
    fields.push((Cow::Owned(key.clone()), BondDataType::BT_STRING));
    //                       ^^^^^^^^^^^ UNNECESSARY CLONE!
}
```

**Problem**: Every log attribute key is cloned when building the schema fields.

**Impact**:
- With 5 attributes per log → 5 string clones per log
- At 10,000 logs/sec → **50,000 string clones/sec**

**Fix**: Use `Cow::Borrowed` instead of `Cow::Owned`:
```rust
// Instead of storing owned String in the map, store &str references
// OR change signature to avoid the clone
fields.push((Cow::Borrowed(key.as_str()), BondDataType::BT_STRING));
```

---

### 🔴 **ISSUE 2: Attribute Map Building - Multiple `to_string()` Calls**
**Location**: Lines 496, 547, 562, 599, 614, 634, 649
**Severity**: HIGH (happens when building attribute maps)

**Line 496** (build_log_attrs_map):
```rust
dict.values()
    .as_any()
    .downcast_ref::<StringArray>()
    .map(|vals| vals.value(key_idx as usize).to_string())
    //                                        ^^^^^^^^^^^ STRING ALLOCATION
```

**Line 547** (build_resource_attrs_map):
```rust
.map(|vals| vals.value(key_idx as usize).to_string())
//                                        ^^^^^^^^^^^ STRING ALLOCATION
```

**Line 562, 599, 614, 634, 649** (extract_attribute_value):
```rust
.map(|vals| vals.value(val_idx as usize).to_string())
//                                        ^^^^^^^^^^^ STRING ALLOCATION
```

**Problem**: Dictionary string values are being copied out as owned `String` instead of borrowed `&str`.

**Impact**:
- **Per-attribute string allocation** for both keys and values
- With 5 attrs/log → 10 allocations per log (5 keys + 5 values)
- At 10,000 logs/sec → **100,000 string allocations/sec**

**Fix**: Change map data structures to use `&str` or `Cow<str>`:
```rust
// Option 1: Use string references (lifetime complexity)
HashMap<u16, Vec<(&'a str, u8, usize)>>

// Option 2: Use Cow for on-demand cloning
HashMap<u16, Vec<(Cow<'a, str>, u8, usize)>>

// Option 3: Store indices instead of strings, dereference later
HashMap<u16, Vec<(usize, u8, usize)>>  // (key_idx, type, value_idx)
```

---

### 🟡 **ISSUE 3: Event Name String Allocation**
**Location**: Line 218
**Severity**: MEDIUM (once per batch, not per log)

```rust
event_name: Arc::new(event_name_str.to_string()),
//                                   ^^^^^^^^^^^ STRING ALLOCATION
```

**Impact**: Allocates per batch (not per log), so lower priority

**Fix**: Use `Arc::from()` or pass ownership:
```rust
event_name: Arc::from(event_name_str),
```

---

### 🟡 **ISSUE 4: Format! for Error Messages**
**Location**: Line 250
**Severity**: LOW (only on error path)

```rust
format!("compression failed: {e}")
```

**Impact**: Only happens on errors, not critical

---

### 🟢 **ISSUE 5: Debug `eprintln!()` Allocations**
**Location**: Lines 366, 373, 377, 381, 386, etc.
**Severity**: MEDIUM-HIGH (many calls per log in debug mode)

```rust
eprintln!("🔍 DEBUG [Row {}] Starting Bond encoding", row_idx);
eprintln!("  📝 {} = {}", FIELD_ENV_NAME, self.env_name);
// ... many more
```

**Problem**: Same as geneva_exporter.rs - debug prints should be conditional

**Fix**: Use the same conditional logging approach:
```rust
#[cfg(feature = "verbose-logging")]
macro_rules! debug_log {
    ($($arg:tt)*) => { eprintln!($($arg)*) };
}

#[cfg(not(feature = "verbose-logging"))]
macro_rules! debug_log {
    ($($arg:tt)*) => {{}};
}
```

---

## Performance Impact Summary

| Issue | Frequency | Allocations at 10K logs/sec |
|-------|-----------|------------------------------|
| Attribute key cloning (line 325) | Per attribute | ~50,000/sec |
| Attribute map building (lines 496+) | Per attribute | ~100,000/sec |
| Event name (line 218) | Per batch | ~100/sec |
| Debug eprintln (many lines) | Per log | **~50,000/sec** |
| **TOTAL** | | **~200,000/sec** |

---

## Recommended Fixes (Priority Order)

### 1️⃣ **HIGH: Fix Attribute String Allocations**

**Current approach**: Store owned `String` in hashmaps
```rust
HashMap<u16, Vec<(String, u8, usize)>>  // Allocates String per attribute
```

**Better approach**: Store indices, dereference when needed
```rust
// Store indices only
HashMap<u16, Vec<(u32, u8, usize)>>  // (key_idx, type, value_idx)

// Dereference when writing to Bond
fn get_key_str(&self, dict: &DictionaryArray, key_idx: u32) -> &str {
    dict.values().as_string::<i32>().value(key_idx as usize)
}
```

**Impact**: Eliminates **~150,000 allocations/sec**

---

### 2️⃣ **HIGH: Add Conditional Debug Logging**

**Add feature flag** (same as geneva_exporter):
```rust
#[cfg(feature = "verbose-logging")]
macro_rules! debug_log {
    ($($arg:tt)*) => { eprintln!($($arg)*) };
}

#[cfg(not(feature = "verbose-logging"))]
macro_rules! debug_log {
    ($($arg:tt)*) => {{}};
}

// Then replace:
eprintln!("🔍 DEBUG [Row {}]...", row_idx);
// With:
debug_log!("🔍 DEBUG [Row {}]...", row_idx);
```

**Impact**: Eliminates **~50,000 allocations/sec** in production

---

### 3️⃣ **MEDIUM: Optimize Cow Usage**

**Line 325**: Don't clone attribute keys
```rust
// Instead of:
fields.push((Cow::Owned(key.clone()), BondDataType::BT_STRING));

// Use (requires lifetime changes):
fields.push((Cow::Borrowed(key), BondDataType::BT_STRING));
```

**Impact**: Eliminates **~50,000 allocations/sec**

---

### 4️⃣ **LOW: Minor Optimizations**

- Line 218: Use `Arc::from()` instead of `Arc::new(to_string())`
- Line 1049: Avoid `data_type().clone()` if possible

---

## Zero-Copy Wins (Keep These!)

✅ **Good practices already in place**:

1. **Direct RecordBatch access** - no copying of Arrow data
2. **Columnar processing** - efficient batch operations
3. **Bond encoding directly from Arrow** - no intermediate structs
4. **Pre-allocated buffers** (line 364): `Vec::with_capacity()`

---

## Proposed Refactoring

### Change attribute map structure

**Before**:
```rust
HashMap<u16, Vec<(String, u8, usize)>>  // Owned strings
```

**After**:
```rust
struct AttrRef {
    key_idx: u32,      // Index into key dictionary
    type_id: u8,       // Attribute type
    attr_idx: usize,   // Row index in attrs batch
}

HashMap<u16, Vec<AttrRef>>  // No string allocations!
```

**Access pattern**:
```rust
// When building schema or writing data:
for attr in attrs_map.get(&log_id) {
    let key_str = get_dict_value(key_dict, attr.key_idx);
    let value = extract_value(attrs_batch, attr.attr_idx, attr.type_id);
    // Use key_str and value without allocating
}
```

---

## Implementation Plan

1. **Phase 1**: Add conditional logging (quick win, low risk)
   - Add `verbose-logging` feature flag
   - Replace `eprintln!()` with `debug_log!()`
   - **Impact**: ~50K fewer allocs/sec

2. **Phase 2**: Refactor attribute maps (bigger change, higher impact)
   - Change HashMap to store indices instead of strings
   - Update all code that accesses the maps
   - **Impact**: ~150K fewer allocs/sec

3. **Phase 3**: Optimize Cow usage (medium complexity)
   - Adjust lifetimes to allow `Cow::Borrowed`
   - **Impact**: ~50K fewer allocs/sec

**Total potential savings**: ~250,000 allocations/sec → **near zero** allocations in hot path! 🎯

---

## Conclusion

The ArrowLogEncoder is the correct encoder being used, and it already has good zero-copy practices for the Arrow data itself. However, **string allocations for attributes** are the main bottleneck:

- 🔴 **~200,000 String allocations/sec** at 10K logs/sec
- 🎯 Can be reduced to **near-zero** with refactoring
- ✅ Low-hanging fruit: Conditional logging → **50K fewer allocs** (easy)
- 🚀 Big win: Index-based attr maps → **150K fewer allocs** (moderate effort)
