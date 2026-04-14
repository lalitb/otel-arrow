# Unified Ingress Sharding Strategies for Shared-Nothing Pipelines

## Related

- `#2364` Windows Networking: multi-threaded consumer support
- `#2186` macOS `SO_REUSEPORT` does not load-balance across listeners

## Summary

Different ingress sources and platforms need different mechanisms to preserve
shared-nothing, core-local processing. The engine should unify the abstraction
for ingress sharding while allowing different concrete mechanisms per source.

Core principle: unify the abstraction, not the mechanism.

## Problem

Some ingress sources cannot rely on kernel-native per-core sharding.

Examples:

- Linux TCP/UDP can use `SO_REUSEPORT`
- Windows/macOS TCP cannot rely on equivalent kernel listener sharding
- Linux `user_events` uses per-CPU perf rings, which preserve ingest locality
  but can create downstream CPU skew
- Some future UDP or non-socket sources may also need explicit redistribution

These cases are related, but they do not share one universal runtime
primitive.

## Background

For Linux `user_events`, there is prior art in the existing C++ implementation
used by AMA/mdsd: a single-threaded all-CPU reader with inline decode. That
validates consolidated ring reading as a workable ingestion model, but it does
not address the scaling needs of a high-throughput export pipeline where decode
and downstream work are materially more expensive than ring draining.

## Goals

- Keep ingress thin before any cross-core handoff
- Preserve core-local processing after the handoff boundary
- Use the earliest safe handoff unit for each source
- Make backpressure and drop semantics explicit
- Give the controller enough metadata to wire the correct strategy
- Allow incremental implementation per source/platform

## Non-Goals

- Do not require all ingress strategies to use topics
- Do not require all handoff units to fit inside `OtapPayload`
- Do not solve every UDP/platform combination in v1
- Do not finalize a universal plugin/extensibility model for future handoff
  types in v1

## Common Model

All strategies fit the same shape:

`ingress -> handoff/rebalance boundary -> core-local processing`

The engine supports multiple ingress sharding strategies:

- `kernel_sharded`
  Kernel distributes ingress directly across per-core listeners/workers
- `socket_handoff`
  A shared listener accepts connections and transfers ownership once to a
  core-local worker
- `raw_batch_handoff`
  Ingress reads/drains source data and hands off typed raw batches before
  expensive decode/transform work

`raw_batch_handoff` is the precise term for the `user_events` case. If later
needed, it can sit under a broader umbrella concept such as `data_handoff`.

## Topology Shapes

The controller/runtime must support multiple topologies:

- `kernel_sharded`: `N -> N`
  Example: Linux sockets with `SO_REUSEPORT`
- `socket_handoff`: `1 -> N`
  Example: Windows/macOS TCP shared listener handing accepted sockets to
  per-core runtimes
- `raw_batch_handoff`: `N -> M`
  Example: Linux `user_events`, where `N` per-CPU readers feed `M` decode
  workers

The ratio is not fixed and must not be assumed to be `1 -> N` everywhere.

```text
kernel_sharded (N->N)
CPU0 -> Pipeline 0
CPU1 -> Pipeline 1
CPU2 -> Pipeline 2

socket_handoff (1->N)
Listener -> dispatch -> Core 0 pipeline
                    -> Core 1 pipeline
                    -> Core 2 pipeline

raw_batch_handoff (N->M)
CPU0 ring --\
CPU1 ring ---+-> handoff -> Decode 0 -> downstream
CPU2 ring ---+            -> Decode 1 -> downstream
CPU3 ring --/
```

## Strategy Matrix

| Source | Strategy | Handoff unit | Core-local phase |
| --- | --- | --- | --- |
| Linux TCP/UDP (`SO_REUSEPORT`) | `kernel_sharded` | none | full socket lifecycle |
| Windows/macOS TCP | `socket_handoff` | accepted socket / connection ownership | read -> decode -> process -> export |
| Linux `user_events` | `raw_batch_handoff` | typed batch of raw userevents records | decode -> Arrow encode -> process -> export |
| UDP on non-Linux platforms | deferred | — | — |

macOS is included here because although it exposes `SO_REUSEPORT`
syntactically, it does not distribute connections across listeners in the way
Linux does. In practice, traffic is delivered to one socket, so macOS belongs
under `socket_handoff`, not `kernel_sharded`.

## Placement in Engine

The strategy declaration must live in a place the controller can inspect during
wiring.

Two likely options:

- extend `ReceiverFactory`
- extend or complement `WiringContract`

`ReceiverFactory` is likely the more natural home, since sharding strategy is a
property of the source implementation rather than a topological wiring rule.
This will be confirmed as the first design decision in the engine placement
sub-task of v1.

Illustrative shape only:

```rust
enum IngressShardingStrategy {
    KernelSharded,
    SocketHandoff,
    RawBatchHandoff,
}
```

This enum is conceptual. Exact placement in engine types is an explicit design
item for v1.

## Handoff Units

The engine should transfer the earliest safe ownership unit for each strategy.

- `socket_handoff`
  Transfers connection ownership, not decoded telemetry
- `raw_batch_handoff`
  Transfers typed raw batches before expensive decode work

For `user_events`, the handoff unit should reuse the existing raw record shape
rather than inventing an untyped blob. The existing `RawUsereventsRecord` in
`crates/core-nodes/src/receivers/userevents_receiver/session.rs` already
carries the needed decode fields. The batch handoff should therefore be a batch
wrapper around those records, or a zero-copy equivalent, with any required
batch-level metadata.

Do not use:

- `OtlpBytes` as a raw transport
- generic `Opaque(Bytes)` for the `user_events` handoff path in v1

## Why Topics Are Not the Universal Primitive

Topics are not the correct primitive for `socket_handoff`, because the handoff
unit is connection ownership rather than telemetry data.

For `raw_batch_handoff`, however, the existing balanced topic mechanism is a
reasonable interim implementation, and may remain acceptable long-term when
used at batch granularity.

So the architectural requirement is not “all handoffs use topics.” It is: the
engine must support the appropriate bounded handoff primitive per strategy.

In practice:

- topics are acceptable for some batch-level data redistribution cases
- topics should not define the long-term architecture for every ingress
  strategy

If balanced topic is used as an interim `raw_batch_handoff` mechanism, note
that current buffering is message-count-based rather than byte-aware. For
variable-sized raw batches, queue sizing may need byte-aware accounting to
avoid underestimating memory pressure.

## Runtime Primitive Guidance

- `socket_handoff`
  Should use an engine-internal handoff primitive, not topic transport
- `raw_batch_handoff`
  May use:
  - a dedicated internal handoff primitive
  - or the existing balanced topic as an interim implementation

The exact queue mechanism is intentionally left open at the RFC level, but it
must support the topology and semantics of the relevant strategy.

## Backpressure and Loss Semantics

Different strategies fail differently and must expose distinct telemetry.

- `kernel_sharded`
  Existing per-core pipeline/channel behavior applies
- `socket_handoff`
  Full handoff queues stall the acceptor and push pressure back toward OS
  socket backlog
- `raw_batch_handoff`
  Full handoff queues usually cause drop, because upstream sources like perf
  rings are not meaningfully backpressurable

Metrics should distinguish:

- source-level loss
- handoff-queue drops
- downstream saturation
- decode/transform failures

## V1 Concrete Scope

1. Define the ingress sharding model and engine placement
2. Implement `raw_batch_handoff` for Linux `user_events`
3. Implement `socket_handoff` for Windows/macOS TCP
4. Defer UDP on non-Linux platforms
5. Keep initial controller/config behavior conservative rather than promising
   full platform transparency

## V1 Deliverable A: Linux `user_events`

- ingress remains per-CPU and thin
- ingress still owns session lifecycle and late tracepoint registration
- handoff unit is a typed batch of existing `RawUsereventsRecord`
- decode and Arrow encoding move after the handoff boundary
- worker count `M` must be configurable or controller-derived

**Prerequisite:** queue topology for `N -> M` handoff must be decided before
implementation begins, since it determines whether ingress workers target a
shared queue, a dispatcher, or per-worker channels.

This preserves the existing ownership model for session setup and retry
behavior while moving the skewed CPU work later in the pipeline.

## V1 Deliverable B: Windows/macOS TCP

- one shared listener
- accepted sockets handed off once to per-core runtimes
- post-handoff lifecycle remains core-local

## Open Questions

- Where exactly does `IngressShardingStrategy` live in engine metadata:
  `ReceiverFactory`, `WiringContract`, or both?
- What is the concrete internal primitive for `socket_handoff`?
  Candidate directions:
  - per-core `tokio::sync::mpsc::channel<TcpStream>` created at engine startup
  - engine-owned acceptor task dispatching to bounded per-core queues
  - a dedicated engine/effect-handler primitive wrapping the same model
- What queue topology should `raw_batch_handoff` use for `N -> M` cases?
  - shared queue
  - dispatcher plus per-worker queues
  - existing balanced topic as interim
- How is decode worker count `M` configured or derived for
  `raw_batch_handoff`?
- Should future handoff kinds remain an engine-owned closed set, or become
  extensible later?
- How much platform transparency should the controller provide in v1 versus
  later?

## Recommended Position

File this as an umbrella RFC.

Then track at least two concrete sub-issues:

- `user_events` via `raw_batch_handoff`
- Windows/macOS TCP via `socket_handoff`

That keeps the common architecture coherent while allowing the two
implementations to proceed independently once the engine placement for strategy
metadata is decided.
