# Fanout Configuration Testing Guide

This guide explains how to test the fanout implementation using the provided example configurations.

## Overview

The fanout implementation automatically activates when a node has multiple destinations in its `out_ports`. The pipeline factory detects this and creates a `LocalFanoutSender` which uses copy-on-write (COW) optimization to minimize memory allocations.

## Example Configurations

### 1. fanout-simple.yaml - Basic Fanout

**What it demonstrates:**
- Single receiver sending to 3 exporters
- Simplest fanout scenario
- All destinations receive the same data

**Architecture:**
```
fake_receiver
    ├─> noop_1  (gets COW clone)
    ├─> noop_2  (gets COW clone)
    └─> noop_3  (gets moved data, no clone!)
```

**Key behavior:**
- First 2 exporters get cheap clones (just Rc pointer bump)
- Last exporter gets the actual data moved (zero-copy)
- Minimal memory overhead

**How to run:**
```bash
cargo run -- --config configs/fanout-simple.yaml
```

### 2. fanout-complex.yaml - Multi-Stage Fanout

**What it demonstrates:**
- Multiple fanout points in a pipeline
- Receiver fans out to 2 processors
- Each processor fans out to 2 exporters
- Total of 3 fanout operations

**Architecture:**
```
fake_receiver
    ├─> debug_processor
    │   ├─> noop_exporter_1
    │   └─> noop_exporter_2
    └─> filter_processor
        ├─> noop_exporter_3
        └─> noop_exporter_4
```

**Key behavior:**
- 3 separate fanout operations (3 LocalFanoutSenders)
- Each fanout independently optimizes its cloning
- Demonstrates cascading fanout scenarios

**How to run:**
```bash
cargo run -- --config configs/fanout-complex.yaml
```

## How Fanout Works

### Automatic Activation

Fanout is automatically enabled when:
1. A node has multiple destinations in `out_ports`
2. All connected nodes are Local (!Send)
3. The pipeline factory detects this during build

No code changes needed - it just works!

### Channel Selection Logic

```rust
// From PipelineFactory::select_channel_type()

if use_shared_channels {
    if num_destinations > 1 {
        // MPMC for shared multi-consumer
    } else {
        // MPSC for shared single consumer
    }
} else {  // Local channels
    if num_destinations > 1 {
        // ✨ LocalFanoutSender activated here!
        // Creates individual MPSC channels
        // Wraps them in COW-optimized fanout
    } else {
        // Regular MPSC for single consumer
    }
}
```

### Performance Characteristics

**Without fanout (N individual sends):**
- N full clones of data
- N memory allocations
- High CPU overhead

**With fanout (COW optimization):**
- Data marked readonly on first send
- N-1 cheap clones (Rc pointer bump)
- 1 move operation (last consumer)
- Minimal CPU and memory overhead

## Testing Scenarios

### Scenario 1: Verify Fanout Creation

**Goal:** Confirm LocalFanoutSender is created

**Steps:**
1. Run with RUST_LOG=debug
2. Look for channel creation logs
3. Verify multiple receivers are created

**Expected:**
```
Creating LocalFanoutSender with 3 destinations
Created 3 underlying MPSC channels
```

### Scenario 2: Performance Comparison

**Goal:** Measure fanout performance benefit

**Steps:**
1. Run `fake-perf.yaml` (single output baseline)
2. Run `fanout-simple.yaml` (3 outputs with fanout)
3. Compare throughput and memory

**Expected:**
- Similar throughput despite 3x outputs
- Minimal memory increase (COW efficiency)

### Scenario 3: Multi-Stage Fanout

**Goal:** Verify cascading fanout works

**Steps:**
1. Run `fanout-complex.yaml`
2. Monitor all 4 exporters receiving data
3. Verify correct data flow

**Expected:**
- All 4 exporters receive data
- Each fanout independently optimized
- No data corruption or loss

## Verification Checklist

### Build Verification
- [ ] `cargo build --package otap-df-engine` succeeds
- [ ] `cargo test --package otap-df-engine` passes (36 tests)
- [ ] No compilation errors related to fanout

### Runtime Verification  
- [ ] `fanout-simple.yaml` runs without errors
- [ ] All 3 exporters receive data
- [ ] `fanout-complex.yaml` runs without errors
- [ ] All 4 exporters receive data in complex scenario

### Performance Verification
- [ ] Throughput comparable to single-output case
- [ ] Memory usage scales sub-linearly with outputs
- [ ] CPU usage remains reasonable

## Troubleshooting

### Config Not Loading
**Problem:** YAML parsing errors

**Solution:** Verify YAML syntax, check indentation

### Fanout Not Activating
**Problem:** Using MPSC instead of fanout

**Check:**
- Are all nodes Local (!Send)?
- Do you have multiple destinations?
- Is dispatch_strategy set?

### Data Not Received
**Problem:** Exporters not receiving data

**Check:**
- Node names match in destinations
- All nodes defined in config
- No typos in plugin URNs

## Advanced Testing

### Custom Scenarios

Create your own fanout configs by:
1. Copy an example config
2. Add more destinations to `out_ports`
3. Mix different node types
4. Test various data volumes

### Integration Testing

Test fanout with real workloads:
1. Use OTLP receiver instead of fake
2. Connect to actual backends
3. Monitor production metrics
4. Verify data consistency

## Expected Output

When running fanout configs, you should see:
- Nodes starting up successfully
- Data flowing through pipeline
- Multiple exporters receiving data
- No errors or panics
- Graceful shutdown on CTRL-C

## Performance Expectations

**fanout-simple.yaml (3 outputs):**
- Throughput: ~90% of single-output
- Memory: ~1.2x single-output
- CPU: ~1.1x single-output

**fanout-complex.yaml (4 outputs, 3 fanouts):**
- Throughput: ~80% of single-output
- Memory: ~1.5x single-output  
- CPU: ~1.3x single-output

These are rough estimates - actual performance depends on hardware and data characteristics.

## Conclusion

The fanout implementation works transparently with YAML configurations. Simply define multiple destinations and the system automatically optimizes data distribution using COW semantics. Test with the provided configs to verify functionality!
