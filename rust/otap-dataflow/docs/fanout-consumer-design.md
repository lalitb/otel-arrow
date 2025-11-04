# Fanout Consumer Design

**Issue**: [GitHub Issue - Implement Fanout Consumer Pattern](https://github.com/open-telemetry/otel-arrow/issues/)

**Status**: Implementation in progress

**Date**: 2025-01-03

## Overview

Implement OpenTelemetry Collector-style fanout consumer pattern in OTAP Dataflow engine. This allows receivers to automatically fan out to multiple pipelines with smart data cloning based on mutation semantics.

## Background

### OpenTelemetry Collector Model

In the OTel Collector:
- Receivers automatically fan-out to each pipeline
- Pipelines are called in sequence
- The `MutatesData` flag is consulted to isolate pipelines from each other
- Smart cloning: data is cloned only for consumers that need to mutate it
- Fanout consumers are "invisible" - they don't produce metrics themselves

Reference: [Go fanoutconsumer implementation](https://github.com/open-telemetry/opentelemetry-collector/blob/main/internal/fanoutconsumer/logs.go#L43)

### OTAP Dataflow Advantages

OTAP Dataflow has additional mechanisms that can optimize fanout:

1. **`Interests` mechanism**: Components declare their interests (ACKS, NACKS, RETURN_DATA, MUTATION)
2. **`RETURN_DATA` interest**: Enables clone-free sequencing where data is returned via ACK/NACK
3. **Hyper-edges**: Native support for one-to-many connections with dispatch strategies

## Design

### 1. Capabilities System

Add a `Capabilities` struct to describe component behavior:

```rust
/// Capabilities describes the capabilities of a component.
#[derive(Clone, Copy, Debug, Default)]
pub struct Capabilities {
    /// MutatesData is set to true if the component may mutate the data.
    /// When true, the fanout logic will clone data before sending to this component
    /// to ensure isolation from other components in the pipeline.
    pub mutates_data: bool,
}
```

Components (processors, exporters) declare their capabilities:

```rust
#[async_trait(?Send)]
pub trait Processor<PData> {
    // ... existing methods ...

    /// Returns the capabilities of this processor.
    /// Default implementation returns readonly (non-mutating) capabilities.
    fn capabilities(&self) -> Capabilities {
        Capabilities { mutates_data: false }
    }
}
```

### 2. Interest Flags

Extended `Interests` bitflags (already implemented in `crates/engine/src/lib.rs:163-186`):

```rust
bitflags::bitflags! {
    pub struct Interests: u8 {
        const ACKS   = 1 << 0;
        const NACKS  = 1 << 1;
        const ACKS_OR_NACKS = Self::ACKS.bits() | Self::NACKS.bits();
        const RETURN_DATA = 1 << 2;
        const MUTATION = 1 << 3;  // NEW
    }
}
```

### 3. Dispatch Strategy

Add `FanoutSequential` to `DispatchStrategy` enum:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DispatchStrategy {
    Broadcast,      // Send to all (existing)
    RoundRobin,     // Load balance (existing)
    Random,         // Random selection (existing)
    LeastLoaded,    // Send to least loaded (existing)
    FanoutSequential,  // NEW: Sequential with smart cloning
}
```

**Behavior of `FanoutSequential`**:

1. Query all destination nodes for their `Capabilities`
2. Separate destinations into two groups:
   - `mutable`: Components with `mutates_data: true`
   - `readonly`: Components with `mutates_data: false`
3. Apply smart cloning logic:
   - Clone data for all mutating consumers **except the last one**
   - The last mutating consumer can receive the original data (if no readonly consumers)
   - Readonly consumers share the original data (marked readonly if >1)
4. Send in sequence:
   - First: N-1 mutating consumers (with clones)
   - Then: Last mutating consumer (original if no readonly)
   - Finally: All readonly consumers (shared original)

### 4. Channel Selection Logic

Modify `PipelineFactory::select_channel_type()` in `crates/engine/src/lib.rs`:

Current signature:
```rust
fn select_channel_type(
    src_node: &dyn Node<PData>,
    dest_nodes: &Vec<&dyn Node<PData>>,
    buffer_size: NonZeroUsize,
) -> Result<(Sender<PData>, Vec<Receiver<PData>>), Error>
```

Enhanced logic for fanout:
1. Check if any destination has `MUTATION` interest
2. If using `FanoutSequential` strategy:
   - Query each destination's capabilities
   - Create appropriate channel topology
   - Set up smart cloning wrapper if needed

### 5. Smart Cloning Implementation

Two approaches:

#### Approach A: Clone at Send Time (Simpler)

In the effect handler's send logic:
```rust
async fn send_pdata(&self, port: &PortName, data: PData) -> Result<(), Error> {
    match self.dispatch_strategy(port) {
        DispatchStrategy::FanoutSequential => {
            // Smart cloning logic
            let (mutable, readonly) = self.categorize_destinations(port);

            // Clone for all mutating consumers except last
            for i in 0..mutable.len()-1 {
                mutable[i].send(data.clone()).await?;
            }

            // Last mutating consumer gets original (if no readonly)
            if readonly.is_empty() {
                mutable.last().unwrap().send(data).await?;
            } else {
                mutable.last().unwrap().send(data.clone()).await?;
            }

            // Readonly consumers share data
            if readonly.len() > 1 {
                data.mark_readonly();
            }
            for dest in readonly {
                dest.send(data.clone_ref()).await?;
            }
        }
        _ => { /* existing logic */ }
    }
}
```

#### Approach B: RETURN_DATA Optimization (Advanced)

Use the `RETURN_DATA` interest flag for zero-clone sequential processing:

1. Send data to first consumer with `RETURN_DATA` interest
2. Wait for ACK with data returned
3. Send returned data to next consumer
4. Repeat until all consumers processed

This avoids cloning entirely but requires:
- All consumers must support `RETURN_DATA`
- Sequential processing (no parallelism)
- Mutation must be prevented (or each consumer works on a view)

## Implementation Plan

### Phase 1: Core Infrastructure ✅ (Partial)

- [x] Add `MUTATION` flag to `Interests` (crates/engine/src/lib.rs:184)
- [ ] Add `Capabilities` struct to engine
- [ ] Add `capabilities()` method to `Processor` trait (local + shared)
- [ ] Add `capabilities()` method to `Exporter` trait (local + shared)

### Phase 2: Dispatch Strategy

- [ ] Add `FanoutSequential` to `DispatchStrategy` enum (crates/config/src/node.rs)
- [ ] Implement fanout logic in `select_channel_type()` (crates/engine/src/lib.rs)
- [ ] Create fanout-aware channel wrapper or sender logic

### Phase 3: Component Updates

- [ ] Update processors that mutate data to declare `mutates_data: true`:
  - Batch processor
  - Retry processor
  - Attributes processor
  - Filter processor (if it mutates)
- [ ] Update readonly processors to explicitly declare `mutates_data: false`:
  - Debug processor
  - Signal router
  - Noop exporter

### Phase 4: Testing

- [ ] Unit tests for capabilities system
- [ ] Unit tests for smart cloning logic
- [ ] Integration tests: fanout with all mutating consumers
- [ ] Integration tests: fanout with all readonly consumers
- [ ] Integration tests: fanout with mixed mutating/readonly consumers
- [ ] Integration tests: verify mutation isolation
- [ ] Performance tests: compare cloning overhead

### Phase 5: Documentation

- [ ] Update CLAUDE.md with fanout pattern
- [ ] Add example configurations
- [ ] Document best practices for declaring capabilities

## Configuration Examples

### Example 1: Multiple Mutating Pipelines

```yaml
nodes:
  otlp_receiver:
    kind: receiver
    plugin_urn: "urn:otel:otlp:receiver"
    out_ports:
      default:
        destinations: [traces_pipeline, metrics_pipeline, logs_pipeline]
        dispatch_strategy: fanout_sequential

  traces_pipeline:
    kind: processor
    plugin_urn: "urn:otel:batch:processor"  # mutates_data: true
    out_ports:
      default:
        destinations: [traces_exporter]

  metrics_pipeline:
    kind: processor
    plugin_urn: "urn:otel:batch:processor"  # mutates_data: true
    out_ports:
      default:
        destinations: [metrics_exporter]

  logs_pipeline:
    kind: processor
    plugin_urn: "urn:otel:batch:processor"  # mutates_data: true
    out_ports:
      default:
        destinations: [logs_exporter]
```

**Behavior**: Data cloned twice (once for traces_pipeline, once for metrics_pipeline). logs_pipeline receives original.

### Example 2: Mixed Mutating and Readonly

```yaml
nodes:
  receiver:
    kind: receiver
    plugin_urn: "urn:otel:otap:receiver"
    out_ports:
      default:
        destinations: [batch_processor, debug_processor, perf_exporter]
        dispatch_strategy: fanout_sequential

  batch_processor:
    kind: processor
    plugin_urn: "urn:otel:batch:processor"  # mutates_data: true

  debug_processor:
    kind: processor
    plugin_urn: "urn:otel:debug:processor"  # mutates_data: false (readonly)

  perf_exporter:
    kind: exporter
    plugin_urn: "urn:otel:perf:exporter"  # mutates_data: false (readonly)
```

**Behavior**:
- `batch_processor` receives a clone (it mutates)
- `debug_processor` and `perf_exporter` share the original (both readonly)
- Original marked as readonly since 2 readonly consumers share it

### Example 3: All Readonly (No Cloning)

```yaml
nodes:
  receiver:
    kind: receiver
    plugin_urn: "urn:otel:fake:receiver"
    out_ports:
      default:
        destinations: [debug1, debug2, perf]
        dispatch_strategy: fanout_sequential

  debug1:
    kind: processor
    plugin_urn: "urn:otel:debug:processor"  # mutates_data: false

  debug2:
    kind: processor
    plugin_urn: "urn:otel:debug:processor"  # mutates_data: false

  perf:
    kind: exporter
    plugin_urn: "urn:otel:perf:exporter"  # mutates_data: false
```

**Behavior**: All three components share the same data (no clones). Data marked readonly.

## Metrics Consideration

As noted in the OTel Collector implementation, fanout consumers should be "invisible":

- The receiver's "produced" count equals the total messages sent
- Each pipeline's "consumed" count equals the total messages received
- No intermediate "fanout" metrics are generated

This is already achieved in OTAP Dataflow because the fanout logic is embedded in the channel/dispatch mechanism, not as a separate component.

## Future Optimizations

### RETURN_DATA for Zero-Clone Fanout

Advanced optimization using `RETURN_DATA` interest:

1. Send data to first consumer with `Interests::RETURN_DATA`
2. First consumer processes and returns data via ACK
3. Send returned data to second consumer with `Interests::RETURN_DATA`
4. Repeat for all consumers
5. Final consumer doesn't need to return data

**Requirements**:
- All consumers must support `RETURN_DATA`
- Mutations must be prevented (or use immutable views)
- Adds latency due to sequential processing

**Trade-off**: Zero clones vs increased latency

### Read-Only Views

Instead of cloning entire OTAP records, create read-only views:

```rust
enum OtapPayloadRef<'a> {
    Owned(OtapPayload),
    Borrowed(&'a OtapPayload),
}
```

Readonly consumers receive `Borrowed` variants, only mutating consumers get `Owned`.

## References

- [OTel Collector Fanout Consumer (Go)](https://github.com/open-telemetry/opentelemetry-collector/blob/main/internal/fanoutconsumer/logs.go)
- [OTAP Dataflow Interests Mechanism](crates/engine/src/lib.rs:163-186)
- [OTAP Dataflow Context and RETURN_DATA](crates/otap/src/pdata.rs:98-178)
- [Hyper-Edges and Dispatch Strategies](crates/config/src/node.rs:81-93)

## Related Issues

- Issue #1098: Context improvements
- Issue #1218: PData consolidation
- This design addresses the fanout consumer requirement

## Decision Log

### Decision 1: Use `Interests::MUTATION` instead of separate capability query

**Rationale**: The `Interests` mechanism is already used throughout the codebase for declaring component behavior. Adding `MUTATION` as an interest flag is consistent with existing patterns (ACKS, NACKS, RETURN_DATA).

**Alternative Considered**: Separate `capabilities()` method on components.

**Chosen Approach**: Use both - `capabilities()` for static declaration, `Interests::MUTATION` for runtime signaling.

### Decision 2: Implement `FanoutSequential` as dispatch strategy

**Rationale**: Dispatch strategies are the natural place to encode routing logic. This makes fanout behavior explicit in configuration.

**Alternative Considered**: Automatic fanout detection based on multiple destinations.

**Chosen Approach**: Explicit `FanoutSequential` strategy for clarity and control.

### Decision 3: Clone at send time (Approach A) initially

**Rationale**: Simpler to implement and understand. Approach B (RETURN_DATA optimization) can be added later as an optimization.

**Chosen Approach**: Start with Approach A, document Approach B for future work.

## Open Questions

1. **Should fanout logic be in the sender or a separate wrapper?**
   - Leaning toward sender logic for simplicity
   - Could extract to wrapper if it becomes complex

2. **How to handle errors in fanout sequence?**
   - If one destination fails, should we continue to others?
   - Probably yes - each pipeline should be independent

3. **Metrics attribution for cloned data?**
   - Each clone should be attributed to the destination pipeline
   - Original message ID should be preserved for correlation

4. **Should we support async fanout (parallel sending)?**
   - Sequential is simpler and matches OTel Collector
   - Parallel could be added as `FanoutParallel` strategy later

## Testing Strategy

### Unit Tests

1. `test_capabilities_default()` - Default capabilities are readonly
2. `test_capabilities_mutating()` - Components can declare mutation
3. `test_smart_cloning_all_mutating()` - N-1 clones, 1 original
4. `test_smart_cloning_all_readonly()` - No clones, all share
5. `test_smart_cloning_mixed()` - Correct cloning for mixed group
6. `test_readonly_marking()` - Data marked readonly when shared

### Integration Tests

1. `test_fanout_three_pipelines()` - E2E fanout to 3 pipelines
2. `test_mutation_isolation()` - Verify mutations don't leak
3. `test_fanout_metrics()` - Verify metric counting
4. `test_fanout_errors()` - Error in one pipeline doesn't affect others

### Performance Tests

1. `bench_fanout_no_clone()` - All readonly (baseline)
2. `bench_fanout_all_clone()` - All mutating (worst case)
3. `bench_fanout_mixed()` - Mixed mutating/readonly (typical case)
4. Compare with current broadcast behavior

## Implementation Notes

### File Locations

- `crates/engine/src/lib.rs` - Interests, Capabilities, PipelineFactory
- `crates/engine/src/local/processor.rs` - Local processor trait
- `crates/engine/src/shared/processor.rs` - Shared processor trait
- `crates/engine/src/local/exporter.rs` - Local exporter trait
- `crates/engine/src/shared/exporter.rs` - Shared exporter trait
- `crates/config/src/node.rs` - DispatchStrategy enum
- `crates/otap/src/pdata.rs` - OtapPdata cloning support
- `crates/otap/src/*_processor.rs` - Individual processor capabilities

### Compatibility

This change is **backward compatible**:
- Existing dispatch strategies continue to work
- Default capabilities are readonly (safe default)
- Explicit opt-in to `FanoutSequential` strategy
- No changes required to existing pipelines

### Migration Path

For users wanting to adopt fanout:

1. Update pipeline config to use `fanout_sequential`
2. Review processors - identify which ones mutate data
3. Update mutating processors to declare `mutates_data: true`
4. Test pipeline behavior with mutation isolation
5. Monitor metrics to ensure correct attribution

## Conclusion

This design implements OpenTelemetry Collector-style fanout with smart cloning while leveraging OTAP Dataflow's unique `Interests` mechanism. The implementation is backward compatible, opt-in, and sets up future optimizations using `RETURN_DATA` for zero-clone fanout.
