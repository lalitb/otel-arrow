# Sender Type Separation Design

## Problem Statement

The unified `Sender<T>` enum contained both Send (!Send) and Shared (Send) variants:
```rust
pub enum Sender<T> {
    Local(LocalSender<T>),           // !Send - contains Rc
    Shared(SharedSender<T>),         // Send
    LocalFanout(LocalFanoutSender<T>),   // !Send - contains Vec<LocalSender>  
    SharedFanout(SharedFanoutSender<T>), // Send - contains Vec<SharedSender>
}
```

While we validated at runtime that Shared nodes only receive Shared/SharedFanout variants, Rust's type system cannot prove this statically, making the entire enum !Send.

## Impact

- OTAP/OTLP gRPC receivers failed to compile
- Tonic requires Send types for async handlers
- `EffectHandler<T>` contains `HashMap<PortName, Sender<T>>` which became !Send

## Solution: Separate Sender Types

Split the Sender enum into two distinct types that match the runtime semantics:

### LocalSender Enum (for !Send contexts)
```rust
pub enum LocalSender<T> {
    Mpsc(otap_df_channel::mpsc::Sender<T>),
    Mpmc(otap_df_channel::mpmc::Sender<T>),
    Fanout(LocalFanoutSender<T>),
}
```

### SharedSender Enum (for Send contexts)  
```rust
pub enum SharedSender<T> {
    Mpsc(tokio::sync::mpsc::Sender<T>),
    Mpmc(flume::Sender<T>),
    Fanout(SharedFanoutSender<T>),
}
```

## Benefits

1. **Type Safety**: The type system now enforces Send requirements
2. **No Runtime Overhead**: No unsafe code or Arc wrappers needed
3. **Clear Intent**: The type signature shows whether a sender is Send or !Send
4. **Compile-Time Validation**: Impossible to mix Send/!Send in wrong contexts

## Implementation Strategy

1. Define `LocalSenderEnum` and `SharedSenderEnum` in `message.rs`
2. Update `ReceiverWrapper::Local` to use `HashMap<PortName, LocalSenderEnum<PData>>`
3. Update `ReceiverWrapper::Shared` to use `HashMap<PortName, SharedSenderEnum<PData>>`
4. Update `ProcessorWrapper` with same pattern
5. Update `EffectHandler` types to use the appropriate sender enum
6. Update pipeline factory to create the correct sender type

## Migration Notes

- The unified `Sender<T>` enum is kept for backwards compatibility in some contexts
- New code should use `LocalSenderEnum` or `SharedSenderEnum` explicitly
- The `Receiver<T>` enum remains unified as it doesn't have Send constraints in the same way

## Alternative Considered

**Unsafe Send Marker**: Could have used `unsafe impl<T: Send> Send for Sender<T>` with runtime validation, but this:
- Requires unsafe code
- Relies on runtime checks rather than compile-time guarantees  
- Makes the !Send property of Local variants invisible to the type system

The separate types approach is more idiomatic Rust and leverages the type system for correctness.
