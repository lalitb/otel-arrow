# FanoutSender Send/!Send Split - Architectural Decision Record

**Date**: 2025-01-04
**Status**: Implementing
**Context**: Fanout consumer pattern implementation for OTAP Dataflow

## Problem Statement

During implementation of Option A (unified `Sender<PData>` type for all EffectHandlers), we encountered a fundamental Rust type system issue:

### The Type System Conflict

```rust
// Desired structure (doesn't compile):
pub enum Sender<T> {
    Local(LocalSender<T>),        // !Send (contains Rc)
    Shared(SharedSender<T>),      // Send (contains Arc)
    Fanout(FanoutSender<T>),      // ??? Should this be Send or !Send?
}

pub struct FanoutSender<T> {
    senders: Vec<Sender<T>>,      // Can contain BOTH Local and Shared!
    // ...
}
```

**The Problem**:
- `FanoutSender<T>` contains `Vec<Sender<T>>`
- `Sender<T>` can be `Sender::Local(LocalSender<T>)` which contains `Rc<...>` (!Send)
- But `Shared` EffectHandlers must be `Send` to work across threads
- Rust's type system cannot verify at compile time that we only put Send senders into Shared wrappers

### Compilation Errors

When building with unified Sender type, we got:

```
error[E0277]: `Rc<otap_df_channel::mpmc::Channel<OtapPdata>>` cannot be sent between threads safely
   --> crates/engine/src/shared/receiver.rs:111
    |
111 | pub struct EffectHandler<PData> {
    |            ^^^^^^^^^^^^^ `Rc<...>` cannot be sent between threads safely
```

The compiler cannot prove that `Sender<PData>` in a Shared EffectHandler only contains Send types.

## Background: Pipeline Architecture

### 1. **Pipelines CAN Mix Local and Shared Nodes**

From `crates/engine/src/lib.rs:475-482`:

```rust
let source_is_shared = src_node.is_shared();
let any_dest_is_shared = dest_nodes.iter().any(|dest| dest.is_shared());
// Use shared channels if EITHER source OR any destination is Shared
let use_shared_channels = source_is_shared || any_dest_is_shared;
```

**Implication**: A single pipeline can have:
- Shared gRPC receiver → Local processors → Shared exporter
- Local test generator → Shared processors → Local exporter
- Any combination!

### 2. **Real Receivers Are Shared**

Production network receivers are `Send`:
- **OTLPReceiver**: `impl shared::Receiver<OtapPdata>` (gRPC)
- **OTAPReceiver**: `impl shared::Receiver<OtapPdata>` (gRPC)
- **FakeDataGenerator**: `impl local::Receiver<OtapPdata>` (testing only)
- **SyslogCefReceiver**: `impl local::Receiver` (file-based)

**Implication**: Real production scenarios with fanout will use Shared receivers.

### 3. **Fanout Use Cases**

**Testing/Development**:
- `FakeDataGenerator` (Local) → [Debug1, Debug2, Noop] (from fanout-simple.yaml)
- Fast iteration, single-threaded debugging

**Production**:
- `OTLPReceiver` (Shared) → [BatchProcessor, MetricsExporter] (tee/mirror traffic)
- `OTAPReceiver` (Shared) → [ProcessorA, ProcessorB, ProcessorC] (routing pipelines)
- High-throughput multi-threaded scenarios

**Implication**: We need fanout for BOTH Local and Shared contexts.

## Decision: Split FanoutSender into Local and Shared Variants

### The Solution

```rust
pub enum Sender<T> {
    Local(LocalSender<T>),              // !Send
    Shared(SharedSender<T>),            // Send
    LocalFanout(LocalFanoutSender<T>),  // !Send - Vec<LocalSender>
    SharedFanout(SharedFanoutSender<T>),// Send - Vec<SharedSender>
}

// !Send variant
pub struct LocalFanoutSender<T> {
    senders: Vec<LocalSender<T>>,       // Explicitly !Send
    mutable_indices: Vec<usize>,
    readonly_indices: Vec<usize>,
}

// Send variant
pub struct SharedFanoutSender<T> {
    senders: Vec<SharedSender<T>>,      // Explicitly Send
    mutable_indices: Vec<usize>,
    readonly_indices: Vec<usize>,
}
```

### Type Safety Guarantee

Now the type system can verify:
- `LocalFanoutSender` contains only `LocalSender` (!Send) ✓
- `SharedFanoutSender` contains only `SharedSender` (Send) ✓
- Shared EffectHandlers can only receive `Sender::Shared` or `Sender::SharedFanout` ✓
- Local EffectHandlers can only receive `Sender::Local` or `Sender::LocalFanout` ✓

### Pipeline Factory Logic

The factory already chooses between Local/Shared channels based on node types:

```rust
// From lib.rs:475-511
if use_shared_channels {
    // Create SharedFanout when source or dest is Shared
    let fanout_sender = SharedFanoutSender::new(shared_senders, mutable, readonly);
    return Ok((Sender::SharedFanout(fanout_sender), receivers));
} else {
    // Create LocalFanout when all nodes are Local
    let fanout_sender = LocalFanoutSender::new(local_senders, mutable, readonly);
    return Ok((Sender::LocalFanout(fanout_sender), receivers));
}
```

## Alternatives Considered

### Alternative 1: Revert Option A - Keep Separate Sender Types

**Approach**:
```rust
// Receivers use LocalSender or SharedSender directly
pub struct EffectHandler<PData> {
    msg_senders: HashMap<PortName, LocalSender<PData>>,  // No Sender enum
}
```

**Pros**:
- Simpler type system
- No need for split fanout variants
- Faster to implement

**Cons**:
- ❌ Cannot use fanout with Shared receivers (gRPC)
- ❌ Only supports fanout in Local (testing) scenarios
- ❌ Blocks production use cases (OTLP → multiple destinations)
- ❌ Not architecturally scalable

**Verdict**: ❌ **Rejected** - Blocks production fanout scenarios

### Alternative 2: Accept Fanout Limitation - Local Only

**Approach**:
- Keep unified `Sender<PData>` enum
- Only allow `Sender::Fanout` in Local wrappers
- Panic or error if fanout used with Shared nodes

**Pros**:
- Simpler implementation (no split)
- Works for current test scenarios

**Cons**:
- ❌ Runtime failure instead of compile-time safety
- ❌ Cannot fanout from gRPC receivers (production blocker)
- ❌ Confusing error messages for users
- ❌ Configuration would parse but fail at runtime

**Verdict**: ❌ **Rejected** - Poor user experience, blocks production

### Alternative 3: Type Erasure with Dynamic Dispatch

**Approach**:
- Use `Box<dyn FanoutSender>` to hide the Local/Shared distinction
- Add runtime checks for Send safety

**Pros**:
- Unified interface
- No split variants

**Cons**:
- ❌ Runtime overhead (heap allocation, vtable dispatch)
- ❌ Loses compile-time safety
- ❌ Violates zero-allocation design goal
- ❌ Still needs runtime checks

**Verdict**: ❌ **Rejected** - Runtime overhead, loses safety

### Alternative 4: Split FanoutSender (CHOSEN)

**Approach**: Separate `LocalFanoutSender` and `SharedFanoutSender` types

**Pros**:
- ✅ Compile-time type safety guaranteed
- ✅ Zero runtime overhead
- ✅ Supports both Local and Shared fanout scenarios
- ✅ Works with gRPC receivers (production use case)
- ✅ Clear separation of concerns
- ✅ Consistent with existing Local/Shared pattern

**Cons**:
- More code to maintain (two implementations)
- Slightly more complex type system
- Need to update Sender enum matching in multiple places

**Verdict**: ✅ **CHOSEN** - Best balance of safety, performance, and functionality

## Implementation Plan

### Phase 1: Create Split Fanout Types
1. ✅ Document architectural decision (this file)
2. Create `LocalFanoutSender<T>` struct in `message.rs`
3. Create `SharedFanoutSender<T>` struct in `message.rs`
4. Add `LocalFanout` and `SharedFanout` variants to `Sender<T>` enum

### Phase 2: Update Core Logic
5. Update `Sender::send()` to handle both fanout variants
6. Update `Sender::try_send()` to handle both fanout variants
7. Update pipeline factory to create appropriate fanout variant based on `use_shared_channels`

### Phase 3: Update Wrapper Types
8. Update `ReceiverWrapper` to accept new fanout variants
9. Update `ProcessorWrapper` to accept new fanout variants
10. Add pattern matching in `set_pdata_sender()` methods

### Phase 4: Testing
11. Build and verify all changes
12. Run full test suite (`cargo test --workspace`)
13. Test fanout-simple.yaml configuration
14. Document any edge cases discovered

## Testing Strategy

### Unit Tests
- LocalFanoutSender smart cloning behavior
- SharedFanoutSender smart cloning behavior
- Readonly marking for both variants
- Clone count verification

### Integration Tests
- Local receiver → LocalFanout → multiple processors
- Shared receiver (OTLP) → SharedFanout → multiple processors
- Mixed pipeline: Shared receiver → SharedFanout → Local processors

### Configuration Tests
- `fanout-simple.yaml` (Local scenario)
- New: `fanout-grpc.yaml` (Shared scenario)
- Verify proper channel selection in factory

## Documentation Updates

1. **FANOUT_EXPLAINED.md**: Add section on Local vs Shared fanout
2. **FANOUT_TESTING.md**: Add testing guidance for both variants
3. **README.md**: Update with split-fanout architecture
4. **This ADR**: Track implementation progress

## Success Criteria

- ✅ All compilation errors resolved
- ✅ Full test suite passes (375+ tests)
- ✅ fanout-simple.yaml works (Local fanout)
- ✅ New fanout-grpc.yaml works (Shared fanout)
- ✅ Zero runtime overhead vs non-fanout channels
- ✅ Type safety guaranteed by compiler

## References

- Original fanout design: `docs/fanout-consumer-design.md`
- Implementation summary: `docs/fanout-implementation-summary.md`
- Phase 2 completion: `docs/fanout-phase2-complete.md`
- Pipeline factory logic: `crates/engine/src/lib.rs:475-550`
- Receiver types: `crates/otap/src/*_receiver.rs`

---

**Decision Made By**: Development team in collaboration with AI assistant
**Implementation Status**: In progress (Phase 1 complete)
**Last Updated**: 2025-01-04
