# Fanout Design Proposal - Design Choices & Rationale

## Problem Statement

Multi-output pipelines (one source → multiple destinations) currently require N full clones of telemetry data. With high-frequency data streams and multiple consumers, this creates significant memory and CPU overhead.

## Proposed Solution: Copy-on-Write Fanout

Implement COW semantics for Local (!Send) pipelines to minimize cloning overhead when distributing data to multiple consumers.

## Key Design Choices

### 1. Local-Only Fanout (Single-Threaded COW)

**Decision:** Implement fanout optimization only for Local (!Send) pipelines using `Rc<T>` semantics.

**Rationale:**
- Local pipelines are single-threaded → no cross-thread synchronization needed
- `Rc<T>` clone is just a pointer bump (cheap) vs full data clone (expensive)
- Shared pipelines already use MPMC channels which handle multi-consumer correctly (though without COW optimization)
- Thread-safe COW with `Arc<T>` would require additional complexity and synchronization overhead

**Trade-off:** Shared pipelines don't get COW benefits, but they're already designed for thread-safety over raw performance.

### 2. Conservative Readonly Approach

**Decision:** Treat all consumers as readonly by default. Mark data readonly on first send, then clone for N-1 consumers and move to final consumer.

**Rationale:**
- Safe default that works correctly for all consumer types
- Avoids needing to analyze consumer capabilities at pipeline build time
- Still provides significant performance benefit (N-1 cheap clones + 1 move vs N full clones)
- Capability detection can be added later as an optimization without breaking changes

**Trade-off:** Doesn't optimize for cases where we could identify mutable consumers (who could receive moved data earlier), but guarantees correctness.

### 3. Automatic Activation

**Decision:** Automatically activate fanout when pipeline config has multiple destinations. No explicit user configuration.

**Rationale:**
- Zero config burden on users
- Correct-by-default behavior
- Transparent performance improvement
- Existing YAML configs automatically benefit

**Trade-off:** Users can't disable it, but there's no reason they'd want to (it's strictly better than N full clones).

### 4. Type System Enforcement

**Decision:** Use Rust's type system to enforce that Local fanout stays !Send and Shared channels remain Send.

**Rationale:**
- Compile-time safety guarantees
- Prevents accidental misuse (e.g., trying to use !Send fanout in Shared pipeline)
- Clear error messages when types don't match
- No runtime overhead for type checking

**Trade-off:** Requires separating some storage types, but gains strong safety guarantees.

## Performance Expectations

**Before (3 outputs):**
- 3 full data clones
- 3× memory allocations
- High CPU for serialization/cloning

**After (3 outputs):**
- 1 mark-readonly operation
- 2 pointer bumps (cheap clones)
- 1 move (zero-copy)
- Expected: ~90% throughput of single output, ~1.2x memory usage

## Integration Approach

Fanout is transparently integrated into the existing pipeline factory. When building a pipeline:
1. Detect multiple destinations in node configuration
2. Check if all connected nodes are Local (!Send)
3. If yes: create fanout sender automatically
4. If no: use existing MPMC (for Shared) or MPSC (for single destination)

No changes to existing node implementations or user configurations.

## Alternatives Considered

### Alternative 1: Arc-based COW for both Local and Shared
- **Rejected:** Arc synchronization overhead negates benefits in single-threaded case
- Local performance would degrade significantly

### Alternative 2: Always clone, no optimization
- **Rejected:** Unacceptable performance cost for multi-output pipelines
- Defeats purpose of efficient data distribution

### Alternative 3: Capability detection (mutable vs readonly consumers)
- **Deferred:** Adds complexity at build time
- Conservative approach is simpler and still highly beneficial
- Can be added later as enhancement without breaking changes

## Questions for Maintainers

1. **Local-only approach:** Is restricting fanout to Local pipelines acceptable? Should we also pursue Arc-based COW for Shared pipelines despite synchronization costs?

2. **Conservative readonly:** Is the default "all readonly" approach acceptable, or should we prioritize capability detection for identifying mutable consumers?

3. **Automatic activation:** Agree with transparent activation, or prefer explicit user opt-in?

4. **Integration points:** Any concerns about integrating into pipeline factory at build time?

## Testing Approach

Provided example configs (`fanout-simple.yaml`, `fanout-complex.yaml`) demonstrate automatic activation and correct behavior. Full test suite passes with no regressions.

Implementation details available in PR for review once design is approved.
