# OTAP Dataflow - Claude Code Documentation

**Last Updated**: 2025-01-04

This document provides context for AI assistants (like Claude) working on this codebase.

## Project Overview

**OTAP Dataflow** is a high-performance OpenTelemetry data pipeline engine written in Rust, implementing the OTAP (OpenTelemetry Protocol with Apache Arrow) protocol for efficient telemetry data processing.

### Key Characteristics

- **Thread-per-Core Architecture**: Uses Tokio's `LocalRuntime` for single-threaded async execution per core
- **Zero-Copy Data Flow**: Minimizes allocations through smart cloning and readonly marking
- **Pipeline-Based Processing**: Receivers → Processors → Exporters with flexible routing
- **High Performance**: Optimized for low latency and high throughput telemetry workloads

## Codebase Structure

```
otap-dataflow/
├── crates/
│   ├── engine/          # Core pipeline engine and runtime
│   │   ├── src/
│   │   │   ├── message.rs        # Message passing, Sender/Receiver, FanoutSender
│   │   │   ├── lib.rs            # Pipeline factory, channel selection
│   │   │   ├── control.rs        # Control messages, node lifecycle
│   │   │   ├── node.rs           # Node trait, capabilities
│   │   │   ├── pipeline_ctrl.rs  # Pipeline control message manager
│   │   │   └── testing/          # Test utilities
│   │   └── tests/
│   │       ├── test_capabilities.rs        # Capabilities tests
│   │       └── test_fanout_behavior.rs     # Fanout behavior tests
│   ├── otap/            # OTAP-specific components
│   │   ├── src/
│   │   │   ├── pdata.rs              # OtapPdata data structure
│   │   │   ├── otap_batch_processor.rs
│   │   │   ├── debug_processor.rs
│   │   │   └── ...
│   ├── config/          # Configuration parsing and validation
│   ├── controller/      # Controller for managing pipelines across cores
│   ├── channel/         # Custom MPSC channel implementations
│   ├── telemetry/       # Metrics and observability
│   └── ...
└── docs/
    ├── architecture.md
    ├── fanout-consumer-design.md
    ├── fanout-implementation-summary.md
    └── fanout-phase2-complete.md
```

## Recent Major Features

### Fanout Consumer Pattern (January 2025)

**Status**: ✅ Complete and tested

The fanout consumer pattern enables smart cloning for receivers that fan-out to multiple pipelines, optimized based on component mutation semantics.

#### Key Components

1. **`ReadonlyMarkable` Trait** (`crates/engine/src/message.rs:88-102`)
   ```rust
   pub trait ReadonlyMarkable {
       fn mark_readonly(&mut self);
   }
   ```
   - Implemented by data types that can be marked readonly
   - Used to prevent mutation when data is shared

2. **`FanoutSender`** (`crates/engine/src/message.rs:104-220`)
   ```rust
   pub struct FanoutSender<T> {
       senders: Vec<Sender<T>>,
       mutable_indices: Vec<usize>,
       readonly_indices: Vec<usize>,
   }
   ```
   - Smart cloning: clones for N-1 consumers, last gets original
   - Marks data readonly when multiple readonly consumers share it
   - Zero-copy optimization for last consumer

3. **Capabilities System** (`crates/engine/src/lib.rs:188-214`)
   ```rust
   pub struct Capabilities {
       pub mutates_data: bool,
   }
   ```
   - Components declare if they mutate data
   - Used by fanout logic to determine cloning strategy
   - Default is `mutates_data: false` (readonly, safe default)

4. **Dispatch Strategies** (`crates/config/src/node.rs:93-109`)
   ```rust
   pub enum DispatchStrategy {
       Broadcast,
       RoundRobin,
       Random,
       LeastLoaded,
       FanoutSequential,  // NEW - enables smart cloning
   }
   ```

#### How Fanout Works

1. **Configuration**: Pipeline specifies `dispatch_strategy: FanoutSequential`
2. **Capability Query**: Engine queries all destination nodes for `mutates_data`
3. **Channel Creation**: Individual channels created for each destination
4. **FanoutSender Creation**: Wraps channels with capability metadata
5. **Smart Send**:
   - Clones for all mutating consumers
   - Shares data among readonly consumers
   - Marks readonly when multiple readonly consumers share
   - Last consumer always gets original (zero-copy)

#### Component Capability Declarations

- **Batch Processor**: `mutates_data: true` (aggregates requests)
- **Debug Processor**: `mutates_data: false` (only reads/logs)
- **All Exporters**: `mutates_data: false` (only send/write)
- **Filter Processor**: `mutates_data: true` (removes items)
- **Attributes Processor**: `mutates_data: true` (modifies attributes)

#### Testing

- **Unit Tests**: `crates/engine/tests/test_capabilities.rs` (6 tests)
- **Behavior Tests**: `crates/engine/tests/test_fanout_behavior.rs` (7 tests)
- **All Tests Pass**: 375 tests, 0 failures

## Architecture Patterns

### Message Passing

The engine uses a message-passing architecture with two channel types:

1. **Local Channels** (`!Send`): For same-thread communication
2. **Shared Channels** (`Send`): For cross-thread communication

Messages are wrapped in `Message<PData>` enum:
```rust
pub enum Message<PData> {
    PData(PData),
    Control(NodeControlMsg<PData>),
}
```

### Node Trait

All components (receivers, processors, exporters) implement the `Node` trait:

```rust
#[async_trait::async_trait(?Send)]
pub trait Node<PData> {
    async fn run(&mut self, ctx: &mut NodeContext<PData>) -> Result<(), Error>;
    fn capabilities(&self) -> Capabilities {
        Capabilities { mutates_data: false }  // Safe default
    }
}
```

### Control Messages

Control messages manage node lifecycle:
- **TimerTick**: Periodic triggers (e.g., batch emission)
- **Config**: Dynamic configuration updates
- **Shutdown**: Graceful shutdown with deadline
- **CollectTelemetry**: Metrics collection trigger
- **Ack/Nack**: Acknowledgment and error handling

### Pipeline Factory

`PipelineFactory` builds runtime pipelines from configuration:
- Instantiates receivers, processors, exporters
- Creates channels based on dispatch strategy
- Connects components into processing graph
- Manages control message routing

## Trait Bounds

When working with generic `PData` types, ensure proper bounds:

- **For data messages**: `PData: Clone + ReadonlyMarkable`
- **For pipeline factory**: `PData: 'static + Clone + Debug + ReadonlyMarkable`
- **For controller**: `PData: 'static + Clone + Send + Sync + Debug + ReadonlyMarkable`

## Common Patterns

### Adding a New Processor

1. Implement the `Node` trait
2. Declare capabilities (mutates_data)
3. Handle control messages in `run()` loop
4. Create factory function
5. Register in pipeline configuration

### Adding ReadonlyMarkable to Types

```rust
impl ReadonlyMarkable for MyType {
    fn mark_readonly(&mut self) {
        self.readonly = true;  // Or appropriate marking
    }
}
```

### Testing Components

Use test utilities in `crates/engine/src/testing/`:
- `TestContext`: For exporter tests
- `TestMsg`: Test message type
- `CtrlMsgCounters`: Track control messages

## Build and Test

```bash
# Build entire workspace
cargo build --workspace

# Run all tests
cargo test --workspace

# Run specific test suite
cargo test -p otap-df-engine test_fanout

# Check for errors
cargo clippy --workspace

# Format code
cargo fmt --all
```

## Important Files to Know

- `crates/engine/src/message.rs` - Core message passing, FanoutSender
- `crates/engine/src/lib.rs` - Pipeline factory, channel selection logic
- `crates/engine/src/control.rs` - Control messages, lifecycle management
- `crates/otap/src/pdata.rs` - OtapPdata structure, readonly marking
- `crates/config/src/node.rs` - Configuration structs, dispatch strategies
- `docs/fanout-consumer-design.md` - Fanout design documentation
- `docs/architecture.md` - Overall system architecture

## Code Quality Guidelines

1. **Zero Warnings**: Fix all clippy warnings before committing
2. **Test Coverage**: All new features require tests
3. **Documentation**: Public APIs must be documented
4. **Error Handling**: Use `Result<T, Error>` consistently
5. **Performance**: Minimize allocations, prefer zero-copy
6. **Thread Safety**: Be explicit about `Send`/`!Send` semantics

## Debugging Tips

1. **Enable logging**: Set `RUST_LOG=debug` for verbose output
2. **Check capabilities**: Ensure components declare correct mutation semantics
3. **Verify dispatch strategy**: Check pipeline configuration uses intended strategy
4. **Test isolation**: Use unit type `()` for simple PData in tests
5. **Build errors**: Often related to missing trait bounds (Clone, ReadonlyMarkable)

## Future Enhancements

Potential areas for improvement (see docs/fanout-consumer-design.md):

1. **RETURN_DATA Optimization**: Zero-clone fanout via data return
2. **Dynamic Capabilities**: Runtime capability negotiation
3. **Smart Routing**: Capability-aware routing decisions
4. **Metrics Attribution**: Per-clone metrics tracking

## Contact and Resources

- **Documentation**: `/docs` directory
- **Examples**: `/configs` directory
- **Benchmarks**: `/benchmarks` directory
- **Architecture Docs**: `docs/architecture.md`
- **Design Principles**: `docs/design-principles.md`

---

*This document is maintained to help AI assistants understand the codebase context and make informed contributions.*
