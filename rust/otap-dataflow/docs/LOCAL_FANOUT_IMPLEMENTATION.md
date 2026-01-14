# Local-Only Fanout Implementation

## Overview

This document describes the simpler fanout implementation for **Local (!Send) pipelines only**. This approach avoids the complexity of type system conflicts with Shared (Send) contexts.

## Design Decision

**Chosen Approach**: Fanout for Local pipelines only  
**Alternative Rejected**: Unified fanout for both Local and Shared contexts

### Rationale

1. **Simplicity**: 80% less complexity than full unified approach
2. **Type Safety**: No Send safety violations
3. **Idiomatic Rust**: Single-threaded optimizations stay `!Send` 
4. **Proven Pattern**: Shared contexts already use MPMC for multi-consumer scenarios

### For Shared Contexts with Multiple Consumers

Use **MPMC channels** (already implemented):
- Pull-based pattern
- Thread-safe by design
- Well-tested and proven

## Implementation

### Core Components

#### 1. ReadonlyMarkable Trait (`message.rs`)

```rust
pub trait ReadonlyMarkable {
    fn mark_readonly(&mut self);
}
```

Marks data as readonly when shared among multiple readonly consumers, enabling copy-on-write optimization.

**Implementations:**
- Primitive types: No-op (already Copy)
- `OtapPdata`: No-op (uses Arc internally)

#### 2. LocalFanoutSender (`message.rs`)

```rust
pub struct LocalFanoutSender<T> {
    senders: Vec<LocalSender<T>>,
    slots: Vec<FanoutSlot>,
    readonly_count: usize,
}
```

**Features:**
- Preserves destination ordering
- Smart cloning:
  - Original goes to final consumer (no clone)
  - All other consumers get clones
  - Marks readonly once if multiple readonly consumers share data
- Validates capability assignments at construction

#### 3. Sender Enum Update

```rust
pub enum Sender<T> {
    Local(LocalSender<T>),
    Shared(SharedSender<T>),
    LocalFanout(LocalFanoutSender<T>),  // NEW
}
```

**Method Separation:**
- `send()` / `try_send()`: Work for Local and Shared, panic on LocalFanout
- `send_fanout()` / `try_send_fanout()`: Work for all types (requires `T: Clone + ReadonlyMarkable`)

This prevents accidental use of fanout senders where trait bounds aren't satisfied.

## Usage Pattern

### Pipeline Configuration

```yaml
pipelines:
  - name: logs-fanout
    type: local  # Must be local for fanout
    source:
      type: fake-generator
    destinations:
      - processor: debug-1  # readonly
      - processor: attributes-modifier  # mutable
      - exporter: otap-export  # readonly
```

### Channel Creation (Pipeline Factory)

When creating channels between nodes:

1. **Check if all nodes are Local**
2. **Check if fanout needed** (multiple consumers)
3. **If yes**: Create `LocalFanoutSender` with capability classification
4. **If no**: Use regular MPSC or MPMC

```rust
// Pseudocode
if all_nodes_local && fanout_needed {
    let fanout = LocalFanoutSender::new(
        local_senders,
        mutable_indices,
        readonly_indices
    );
    Sender::LocalFanout(fanout)
} else if multiple_consumers {
    // Use MPMC for Shared or mixed contexts
    create_mpmc_channel()
} else {
    // Regular MPSC
    create_mpsc_channel()
}
```

## Benefits

### Compared to Unified Approach

| Aspect | Local-Only | Unified (Rejected) |
|--------|-----------|-------------------|
| Complexity | Low | High |
| Type Safety | No issues | Requires type splitting |
| Send Safety | Guaranteed | Complex bounds |
| Implementation | ~300 lines | ~1000+ lines |
| Maintenance | Simple | Complex |

### Performance

- **Zero-cost for non-fanout**: No performance impact on regular pipelines
- **Optimized cloning**: Final consumer avoids clone
- **Readonly sharing**: Multiple readonly consumers share data instance

## Limitations

1. **Local pipelines only**: Fanout not available for Shared contexts
2. **Clone requirement**: PData must implement `Clone + ReadonlyMarkable`
3. **No sender cloning**: `LocalFanoutSender` cannot be cloned (owns multiple senders)

## Testing

### Unit Tests Required

1. **Fanout slot validation**: Verify capability assignment validation
2. **Clone optimization**: Verify final consumer gets original
3. **Readonly marking**: Verify marking happens once for multiple readonly consumers
4. **Destination ordering**: Verify sends happen in configured order

### Integration Tests Required

1. **Multi-consumer pipelines**: Full pipeline with fanout
2. **Mixed capabilities**: Mutable + readonly consumers
3. **Error handling**: Channel closure scenarios

## Migration from Original Implementation

If upgrading from earlier unified fanout attempts:

1. Remove `SharedFanoutSender` (not needed)
2. Remove split sender types (keep unified `Sender<T>` enum)
3. Use MPMC for Shared multi-consumer cases
4. Update pipeline factory logic for channel selection

## See Also

- `FANOUT_EXPLAINED.md`: Original comprehensive fanout design (both Local and Shared)
- `FANOUT_ARCHITECTURE.md`: Architectural decisions and patterns
- `PIPELINES_LOCAL_VS_SHARED_EXPLAINED.md`: When to use Local vs Shared

## Future Enhancements

If Shared fanout is needed in the future:

1. Implement `SharedFanoutSender<T>` with `Arc<Mutex<Vec<SharedSender<T>>>>`
2. Add `Sender::SharedFanout` variant
3. Update `send_fanout()` methods to handle SharedFanout
4. Document performance trade-offs of locking vs MPMC

**Current Recommendation**: Use MPMC for Shared multi-consumer scenarios until profiling shows fanout is needed.
