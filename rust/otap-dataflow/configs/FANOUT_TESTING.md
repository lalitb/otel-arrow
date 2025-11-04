# Testing FanoutSender with Smart Cloning

This guide shows you how to independently test the FanoutSender implementation.

---

## Quick Start

### 1. Simple Example (3 readonly processors)

```bash
# From the otap-dataflow root directory
cargo run --bin df_engine -- --config configs/fanout-simple.yaml --cores 1
```

**What happens**:
- Receiver generates 50 log signals at 5/second
- FanoutSender distributes to 3 readonly processors
- **Clone count**: 2 (debug1 + debug2)
- **Original goes to**: noop (last consumer, zero-copy!)
- You'll see debug output from both debug processors

**Press Ctrl+C to stop**

---

### 2. Full Example (mixed mutating + readonly)

```bash
cargo run --bin df_engine -- --config configs/fanout-example.yaml --cores 1
```

**What happens**:
- Receiver generates 100 log signals at 10/second
- FanoutSender distributes to 4 consumers:
  1. **Batch processor** (mutating) - gets clone
  2. **Debug1** (readonly) - gets clone + readonly mark
  3. **Debug2** (readonly) - gets clone + readonly mark
  4. **Noop exporter** (readonly) - gets ORIGINAL
- **Clone count**: 3
- Batch processor aggregates data every 1 second
- Debug processors show different verbosity levels

**Press Ctrl+C to stop**

---

## What to Look For

### Console Output

You'll see output like:

```
[INFO] Pipeline starting on core 0
[DEBUG] Debug1: Received signal batch with 5 logs
[DEBUG] Debug2: Received signal batch with 5 logs
[INFO] Batch processor: Emitting batch of 50 logs
```

### Smart Cloning in Action

The key behavior to observe:

1. **Multiple readonly processors** - They all receive data without issues
2. **Batch processor independently modifies** - Its aggregation doesn't affect debug processors
3. **No data corruption** - Each processor sees correct, independent data
4. **Performance** - Zero extra clones for the last consumer

---

## Comparing with Other Strategies

### Round Robin (default)

```yaml
dispatch_strategy: round_robin  # Each message goes to ONE destination
```

**Behavior**: Messages distributed round-robin style
- Message 1 → debug1
- Message 2 → debug2
- Message 3 → noop
- Message 4 → debug1 (repeats)

### Broadcast (shared memory - broken for mutating)

```yaml
dispatch_strategy: broadcast  # All get SAME data (shared)
```

**Behavior**: ❌ All processors share the same data object
- If one mutates, all see the mutation → **data corruption!**
- This is why FanoutSequential was needed

### FanoutSequential (smart cloning)

```yaml
dispatch_strategy: fanout_sequential  # Smart cloning based on capabilities
```

**Behavior**: ✅ Each processor gets appropriate data
- Mutating processors: Independent clones
- Readonly processors: Can share (marked readonly)
- Last consumer: Gets original (zero-copy)

---

## Testing Different Scenarios

### Scenario 1: All Readonly Processors

Edit `fanout-simple.yaml` to use only debug processors:

```yaml
destinations:
  - debug1
  - debug2
  - debug3
```

**Expected**: 2 clones, last gets original

---

### Scenario 2: All Mutating Processors

Create a config with multiple batch processors:

```yaml
destinations:
  - batch1
  - batch2
  - batch3
```

**Expected**: 2 clones, last gets original (each batches independently)

---

### Scenario 3: Mixed (current examples)

```yaml
destinations:
  - batch      # Mutating
  - debug1     # Readonly
  - debug2     # Readonly
  - noop       # Readonly
```

**Expected**: 3 clones (batch + debug1 + debug2), noop gets original

---

## Verification with Logging

### Enable Debug Logging

Set environment variable:

```bash
RUST_LOG=otap_df_engine=debug cargo run --bin df_engine -- --config configs/fanout-simple.yaml --cores 1
```

Look for messages like:

```
DEBUG otap_df_engine::message: FanoutSender created with 1 mutating, 3 readonly indices
DEBUG otap_df_engine::message: Sending to mutating consumer 0
DEBUG otap_df_engine::message: Sending to readonly consumer 1 (marked readonly)
DEBUG otap_df_engine::message: Sending to readonly consumer 2 (marked readonly)
DEBUG otap_df_engine::message: Last consumer 3 gets original
```

*(Note: Actual debug messages may vary based on implementation)*

---

## Performance Testing

### Measure Clone Count

Run with performance metrics:

```bash
cargo run --release --bin df_engine -- --config configs/fanout-simple.yaml --cores 1
```

### Compare Strategies

1. Test with `fanout_sequential` - observe smart cloning
2. Change to `broadcast` - observe shared data
3. Change to `round_robin` - observe distribution

---

## Troubleshooting

### "No such file" error

Make sure you're in the `otap-dataflow` root directory:

```bash
cd /path/to/otap-dataflow
cargo run --bin df_engine -- --config configs/fanout-simple.yaml --cores 1
```

### Build errors

Clean and rebuild:

```bash
cargo clean
cargo build --release
cargo run --release --bin df_engine -- --config configs/fanout-simple.yaml --cores 1
```

### No output

- Check that `max_signal_count` is not 0
- Verify `signals_per_second` is reasonable (1-100)
- Ensure debug processors have `verbosity: basic` or `detailed`

---

## Advanced Testing

### Test with Real OTAP/OTLP Data

Instead of fake data generator, use real receiver:

```yaml
receiver:
  kind: receiver
  plugin_urn: "urn:otel:otap:receiver"
  out_ports:
    out_port:
      destinations:
        - batch
        - debug1
        - debug2
      dispatch_strategy: fanout_sequential
  config:
    endpoint: "127.0.0.1:4317"
```

Then send data with:

```bash
# Use any OTLP/OTAP client to send to localhost:4317
```

---

## What You're Testing

When you run these examples, you're verifying:

✅ **FanoutSender creation** - Correctly identifies mutating vs readonly processors
✅ **Smart cloning** - Clones only when necessary
✅ **Zero-copy optimization** - Last consumer gets original
✅ **Readonly marking** - Multiple readonly consumers share safely
✅ **Capability-based routing** - Respects component capabilities
✅ **No data corruption** - Each processor sees independent data

---

## Next Steps

After testing:

1. Check `docs/FANOUT_EXPLAINED.md` for deep dive
2. Review `docs/IMPLEMENTATION_CHANGES_SUMMARY.md` for what changed
3. See `CLAUDE.md` for overall architecture

---

**Questions?** See `docs/FANOUT_EXPLAINED.md` Part 6.5 "Known Limitations"
