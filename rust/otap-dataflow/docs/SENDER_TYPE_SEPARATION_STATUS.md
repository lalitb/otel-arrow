# Sender Type Separation - Implementation Status

## Completed Work

1. ✅ Created `LocalSenderEnum` and `SharedSenderEnum` in `message.rs`
2. ✅ Added documentation explaining the design decision
3. ✅ Updated imports in `receiver.rs`

## Remaining Work

### Phase 1: Update Core Engine Types

1. **ReceiverWrapper** (`crates/engine/src/receiver.rs`)
   - Change `Local` variant: `pdata_senders: HashMap<PortName, LocalSenderEnum<PData>>`
   - Change `Shared` variant: `pdata_senders: HashMap<PortName, SharedSenderEnum<PData>>`
   - Update `set_pdata_sender` to accept `LocalSenderEnum` or `SharedSenderEnum`
   - Update `start()` method to pass correct sender enum to EffectHandler

2. **ProcessorWrapper** (`crates/engine/src/processor.rs`)
   - Same changes as ReceiverWrapper for `pdata_senders` field
   - Update `set_pdata_sender` implementation
   - Update `start()` method

3. **EffectHandler** (`crates/engine/src/local/receiver.rs` and `crates/engine/src/shared/receiver.rs`)
   - Local EffectHandler: Change `msg_senders: HashMap<PortName, LocalSenderEnum<PData>>`
   - Shared EffectHandler: Change `msg_senders: HashMap<PortName, SharedSenderEnum<PData>>`
   - Update `new()` constructor
   - Update `send_message()` implementation
   - Same changes for processor EffectHandlers

### Phase 2: Update Pipeline Factory

4. **Pipeline Factory** (`crates/engine/src/lib.rs`)
   - Update `select_channel_type()` return type to return separate enum types
   - Create two versions or make it generic over the return type
   - Update channel assignment logic to use correct enum variant

5. **NodeWithPDataSender Trait** (`crates/engine/src/node.rs`)
   - Consider splitting into `LocalNodeWithPDataSender` and `SharedNodeWithPDataSender`
   - Or make `set_pdata_sender` generic to accept either enum type

### Phase 3: Fix OTAP/OTLP Issues

6. **Add ReadonlyMarkable trait** for OTLPData and OtapPdata
   - Implement `ReadonlyMarkable` for these types in their respective modules
   - This will fix the trait bound errors in gRPC servers

7. **Test Code Updates**
   - Update test helper functions to use correct sender enum types
   - Fix type mismatches in signal_type_router.rs and syslog_cef_receiver.rs tests

## Type Signature Changes Summary

### Before (Unified)
```rust
// ReceiverWrapper::Local
pdata_senders: HashMap<PortName, Sender<PData>>

// ReceiverWrapper::Shared  
pdata_senders: HashMap<PortName, Sender<PData>>  // !Send due to Local variants
```

### After (Separated)
```rust
// ReceiverWrapper::Local
pdata_senders: HashMap<PortName, LocalSenderEnum<PData>>  // !Send explicitly

// ReceiverWrapper::Shared
pdata_senders: HashMap<PortName, SharedSenderEnum<PData>>  // Send guaranteed
```

## Benefits Once Complete

1. **Compile-Time Send Safety**: Rust's type system prevents mixing Send/!Send
2. **Clear Intent**: Type signature shows Send vs !Send immediately  
3. **gRPC Compatibility**: `SharedSenderEnum` is Send, enabling Tonic integration
4. **Zero Runtime Cost**: No Arc wrappers or unsafe code needed

## Estimated Effort

- **Phase 1**: ~2-3 hours (core engine refactoring)
- **Phase 2**: ~1 hour (pipeline factory updates)
- **Phase 3**: ~1 hour (fix OTAP/OTLP and tests)
- **Total**: ~4-5 hours of focused development

## Next Steps

1. Start with Phase 1: Update ReceiverWrapper and ProcessorWrapper types
2. Update EffectHandler types in both local and shared modules  
3. Fix compilation errors as they arise
4. Move to Phase 2 once engine compiles
5. Complete Phase 3 to fix gRPC receivers and tests
6. Run full test suite to verify correctness
