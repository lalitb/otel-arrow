# Fanout Send Safety Resolution

## Problem Summary

During fanout implementation, we encountered a fundamental Send safety issue with Rust's type system:

- `Sender<PData>` enum can contain both `Local` and `Fanout` variants
- `LocalSender` and `LocalFanout` contain `Rc<T>` which is `!Send`
- `shared::Processor` and `shared::Receiver` traits require `Send` futures
- Rust's type system cannot verify at compile time that we only use Send senders in Shared contexts

## Initial Attempt (Unified Sender Enum)

We first tried a unified `Sender<T>` enum:

```rust
pub enum Sender<T> {
    Local(LocalSender<T>),           // !Send
    Shared(SharedSender<T>),         // Send  
    Fanout(FanoutSender<T>),         // Could be either!
}
```

**Problem**: `FanoutSender<T>` internally has `Vec<Sender<T>>`, which could contain `Local` variants, making the whole thing potentially `!Send`.

## Resolution (Split Fanout Variants)

We split the fanout into separate Send and !Send variants:

```rust
pub enum Sender<T> {
    Local(LocalSender<T>),              // !Send
    Shared(SharedSender<T>),            // Send
    LocalFanout(LocalFanoutSender<T>),  // !Send - Vec<LocalSender>
    SharedFanout(SharedFanoutSender<T>), // Send - Vec<SharedSender>
}
```

### Type Safety Guarantees

1. **Local Wrappers** (`ProcessorWrapper::Local`, `ReceiverWrapper::Local`):
   - Accept only `Sender::Local` or `Sender::LocalFanout`
   - Compile-time enforcement in `set_pdata_sender()`
   - Can use `Rc<T>` for efficient !Send channels

2. **Shared Wrappers** (`ProcessorWrapper::Shared`, `ReceiverWrapper::Shared`):
   - Accept only `Sender::Shared` or `Sender::SharedFanout`
   - Compile-time enforcement in `set_pdata_sender()`
   - All internal types are `Send`, enabling multi-threaded operation

### Pipeline Factory Responsibility

The `PipelineFactory` must create the correct sender variant based on the pipeline type:

```rust
// For Local pipelines:
if is_local_pipeline {
    if is_fanout {
        create_local_fanout_sender()  // Uses LocalSender internally
    } else {
        create_local_sender()
    }
}

// For Shared pipelines:
if is_shared_pipeline {
    if is_fanout {
        create_shared_fanout_sender()  // Uses SharedSender internally
    } else {
        create_shared_sender()
    }
}
```

## Testing Implications

### Test Message Type Issue

Our test type `TestMsg` can be used with `LocalFanout` senders (containing `Rc`), which means:

1. `TestMsg` itself doesn't enforce Send safety
2. The `shared::Processor<TestMsg>` and `shared::Receiver<TestMsg>` impls are conditionally compiled out:

```rust
#[cfg(feature = "__disabled_test_shared_processor")]
#[async_trait]
impl shared::Processor<TestMsg> for TestProcessor { ... }
```

3. The corresponding test functions are also conditionally compiled:

```rust
#[cfg(feature = "__disabled_test_shared_processor")]
#[test]
fn test_processor_shared() { ... }
```

### Production Code and Architecture

The overall architecture is **thread-per-core with `LocalSet`** for efficiency. However, certain components have specific requirements:

#### OTAP gRPC Receivers

For gRPC receivers (e.g., `OTAPReceiver`):
- Use `ReceiverWrapper::shared()` because **Tonic's gRPC server requires `Send` types**
- Tonic spawns handler tasks with `tokio::spawn` (not `spawn_local`), requiring `Send`
- The gRPC service implementations (`ArrowLogsServiceImpl`, etc.) are wrapped in `Arc<>` by Tonic
- Therefore, `effect_handler: shared::EffectHandler<OtapPdata>` must be `Send`
- This requires all senders in the effect handler to be `SharedSender` or `SharedFanout`

#### Pipeline Factory Responsibility

The pipeline factory must:
1. Check if the target node uses `ReceiverWrapper::Shared` or `ProcessorWrapper::Shared`
2. If Shared: Create `SharedSender` or `SharedFanout` (Send-safe)
3. If Local: Create `LocalSender` or `LocalFanout` (more efficient, !Send)

**Key Insight**: This isn't about the whole pipeline being multi-threaded. It's about specific nodes (like gRPC servers) that interface with libraries requiring `Send`, while most of the pipeline runs efficiently on a single thread with `LocalSet`.

## Architecture Benefits

1. **Compile-Time Safety**: Type system prevents mixing Send/!Send senders
2. **Zero Runtime Cost**: No runtime checks needed for Send safety
3. **Clear Semantics**: Local vs Shared distinction is explicit in types
4. **Flexible**: Supports both single-threaded (efficient) and multi-threaded (scalable) pipelines

## Future Enhancements

To fully test Shared variants:

1. Create a separate `SendTestMsg` type that's explicitly `Send + Sync`
2. Implement `shared::Processor<SendTestMsg>` for test processor
3. Implement `shared::Receiver<SendTestMsg>` for test receiver  
4. Add tests using `SendTestMsg` to verify Shared pipeline behavior

This would provide test coverage while maintaining type safety guarantees.
