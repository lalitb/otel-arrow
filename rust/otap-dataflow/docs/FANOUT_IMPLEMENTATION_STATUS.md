# Fanout Implementation Status

## ✅ Completed (Core Foundation)

### 1. Core Fanout Components (`message.rs`)
- ✅ `ReadonlyMarkable` trait
- ✅ `LocalFanoutSender<T>` implementation
- ✅ `FanoutSlot` for capability tracking
- ✅ `Sender::LocalFanout` enum variant
- ✅ Method separation: `send()` vs `send_fanout()` with trait bounds

### 2. PData Integration (`pdata.rs`)
- ✅ `ReadonlyMarkable` implemented for `OtapPdata`

### 3. Pipeline Factory Integration (`lib.rs`)
- ✅ `select_channel_type()` creates `LocalFanoutSender` for local multi-consumer
- ✅ `ReadonlyMarkable` trait bound added to `PipelineFactory`
- ✅ Automatic fanout detection based on node types

### 4. Documentation
- ✅ `LOCAL_FANOUT_IMPLEMENTATION.md` - Complete guide
- ✅ Design rationale and trade-offs documented
- ✅ Usage patterns explained

### 5. Build Status
- ✅ Engine crate compiles successfully
- ✅ No fanout-related compilation errors

## ⚠️ Identified Issue: Sender Storage

### Problem

The `ProcessorWrapper` and `ReceiverWrapper` currently store `LocalSender<PData>` or `SharedSender<PData>` directly:

```rust
ProcessorWrapper::Local {
    pdata_senders: HashMap<PortName, LocalSender<PData>>,  // ← Problem
    ...
}
```

But `set_pdata_sender()` receives `Sender<PData>` which can now be:
- `Sender::Local(LocalSender<PData>)` 
- `Sender::Shared(SharedSender<PData>)`
- `Sender::LocalFanout(LocalFanoutSender<PData>)` ← **New variant**

The current code unwraps and stores only the inner sender, which won't work for fanout.

### Solution Options

#### Option A: Store `Sender<PData>` Directly (Recommended)

Change wrapper fields to store the full `Sender` enum:

```rust
ProcessorWrapper::Local {
    pdata_senders: HashMap<PortName, Sender<PData>>,  // ← Store full enum
    ...
}
```

**Pros:**
- Clean, future-proof design
- Works with all sender variants
- Minimal code changes

**Cons:**
- Requires updating `EffectHandler` to accept `Sender` instead of specific types
- Some additional trait bound management

#### Option B: Add Fanout-Aware Unwrapping

Keep current storage but add special handling for fanout:

```rust
match sender {
    Sender::Local(s) => pdata_senders.insert(port, s),
    Sender::LocalFanout(f) => {
        // Store wrapped fanout somehow
        // More complex...
    }
    ...
}
```

**Pros:**
- Minimal changes to existing code

**Cons:**
- Hacky, not future-proof
- Complicates send logic
- Doesn't scale well

## 🔧 Remaining Work

### 1. Update Wrapper Storage (Critical)

Files to modify:
- `crates/engine/src/processor.rs`
- `crates/engine/src/receiver.rs`
- `crates/engine/src/local/processor.rs`
- `crates/engine/src/local/receiver.rs`  
- `crates/engine/src/shared/processor.rs`
- `crates/engine/src/shared/receiver.rs`

Changes needed:
1. Change `pdata_senders: HashMap<PortName, LocalSender<PData>>` to `HashMap<PortName, Sender<PData>>`
2. Update `EffectHandler::new()` signatures to accept `HashMap<PortName, Sender<PData>>`
3. Update `send_message()` / `send_message_to()` to call appropriate `Sender` methods
4. Handle fanout with `send_fanout()` method when `Sender::LocalFanout` is used

### 2. Add Capability Detection

Currently, the pipeline factory assumes all destinations are readonly:

```rust
// TODO: Determine mutable vs readonly based on node capabilities
let mutable_indices = Vec::new();
let readonly_indices = (0..num_destinations).collect::<Vec<_>>();
```

Need to:
1. Add capability information to node configs
2. Parse capability from YAML/config
3. Pass to `LocalFanoutSender::new()`

### 3. Testing

Create tests for:
- Unit tests for `LocalFanoutSender`
- Integration tests with multi-consumer pipelines
- Mixed mutable/readonly scenarios
- Error handling

### 4. Example Configurations

Create YAML configs demonstrating:
- Simple fanout (1 source → multiple consumers)
- Mixed capabilities (readonly + mutable)
- Multiple fanout nodes in one pipeline

## 📊 Completion Estimate

- **Core Implementation**: 95% complete
- **Integration**: 60% complete (wrapper storage needs fixing)
- **Testing**: 0% complete
- **Documentation**: 90% complete
- **Examples**: 0% complete

**Overall**: ~65% complete

## 🎯 Next Steps (Priority Order)

1. **Fix wrapper storage** (Critical - blocks functionality)
2. **Add capability detection** (High - needed for full feature)
3. **Create integration tests** (High - validates correctness)
4. **Add example configs** (Medium - aids adoption)
5. **Performance benchmarks** (Low - optimization)

## 🏗️ Architecture Decision Recap

**Chosen**: Local-only fanout with `Sender` enum

**Rationale**:
- 80% less complexity than unified Send/!Send approach
- No type system conflicts
- Idiomatic Rust (keeps !Send optimizations)
- Proven alternative (MPMC) for Shared contexts

**Trade-offs Accepted**:
- Fanout only for Local pipelines
- Must implement `Clone + ReadonlyMarkable`
- Can't clone fanout sender (owns multiple senders)

## 📚 Related Documentation

- `LOCAL_FANOUT_IMPLEMENTATION.md` - Implementation guide
- `FANOUT_EXPLAINED.md` - Original comprehensive design
- `FANOUT_ARCHITECTURE.md` - Architectural decisions
