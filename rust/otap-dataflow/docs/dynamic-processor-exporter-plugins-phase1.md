# RFC: Dynamic Processor and Exporter Plugins for `otap-dataflow` Phase 1

## Status

**Superseded for the hot path by native cdylib plugins.**

> The Wasmtime-component implementation described below remains in the
> tree (under `crates/plugin-{api,manifest,host,nodes}`) but is **parked
> for hot-path use**: its `process(list<u8>) → list<u8>` ABI copies and
> re-encodes payloads, which is incompatible with the OTAP/Arrow
> zero-copy requirement. See [`crates/plugin-native-host`] and
> [`crates/plugin-native-nodes`] for the supported phase-1 zero-copy
> path: native cdylib processor plugins receiving an opaque
> `OtapPdataHandle` and returning `ForwardSame` / `Drop` / `Error`.
>
> The wasm scaffolding (manifest discovery, SHA-256 + minisign,
> descriptor / `validate-config`, `PluginFingerprint`, runtime registry
> overlay) is shared between the two backends and is not deprecated. A
> future revision may revive a wasm path with a resource-handle ABI for
> sandboxed scripting plugins, where data never crosses linear memory.

**Phase 1 implemented.** The design below was the original RFC; the
"Implementation Status" section immediately following lists what
currently ships on the `wasm` branch. Subsequent sections describe
both the design and (where applicable) the as-built behavior.

## Implementation Status (phase 1, current)

The following capabilities are implemented end-to-end and exercised by
unit and integration tests on the `wasm` branch:

- Plugin discovery from `--plugin-dir <DIR>` (repeatable) at startup.
- Manifest parsing (`apiVersion: otap.plugin/v1alpha1`,
  `kind: WasmPlugin`) with mandatory `runtime.path` + `runtime.sha256`.
- SHA-256 artifact integrity verification (always required).
- Real **minisign** signature verification with two host policies:
  default (verify when a public key is configured) and
  `--plugin-require-signed` (reject any plugin without a verified
  signature).
- Wasmtime Component Model loading, behind the
  `wasmtime-backend` feature flag on `crates/plugin-host`.
- Disk-backed precompiled component cache keyed by artifact SHA-256,
  Wasmtime version, target triple, engine-config fingerprint, and
  plugin API version (`--plugin-cache-dir <DIR>`).
- `descriptor()` and `validate-config()` plugin exports invoked at
  startup; descriptor is authoritative for component URNs / kinds /
  payload-format support.
- `WasmProcessorAdapter`: real runtime path with host-enforced
  Wasmtime epoch-interruption deadline and OTLP-bytes payload
  conversion. Preserves `OtapPdata` context across the plugin call.
- `WasmExporterAdapter`: real runtime path dispatched through
  `tokio::spawn_blocking`, with an explicit cross-instance concurrency
  cap (`--plugin-exporter-blocking-concurrency`, default 32) backed by
  a shared `tokio::sync::Semaphore`. Per-instance serialization is
  preserved by the engine's sequential dispatch model.
- Plugin fingerprint
  `(component_urn, plugin_version, artifact_sha256, plugin_api_version)`
  participates in live-reconfig rollout planning: a change to any
  component referenced by a pipeline forces `RolloutAction::Replace`.
- Phase-1 invariants enforced at descriptor-load time:
  processors/exporters only; `otlp-proto-bytes` payload only;
  single/default-output only. `otap-arrow-ipc` is reserved in the WIT
  variant but rejected.
- Static `linkme` registration is unchanged and remains the default
  path; pipelines may freely mix static and plugin-backed nodes.

What is **not** implemented in phase 1 (deferred):

- Receiver and extension plugins.
- Arrow IPC (`otap-arrow-ipc`) execution; reserved in the ABI, rejected
  at load time.
- Remote plugin fetch / auto-update.
- In-place code unload/reload at runtime.
- Replacement of built-in static nodes by plugins of the same URN
  (static-first precedence).
- Metrics import API for plugins (logs only in v1).

## Problem Statement

`otap-dataflow` currently supports only statically linked components. The binary in `rust/otap-dataflow/src/main.rs` side-effect imports core and contrib crates so their `linkme` registrations are visible to `OTAP_PIPELINE_FACTORY`. The factory is declared in `rust/otap-dataflow/crates/otap/src/lib.rs`, and macro expansion comes from `rust/otap-dataflow/crates/engine-macros/src/lib.rs`.

This means:

- new processors/exporters require rebuilding the collector binary
- registration is link-time only
- config validation assumes statically registered validators
- runtime identity does not include external component artifacts
- there is no supported runtime extension surface for plugin components

We want an additive, opt-in dynamic plugin path that preserves the current static path.

## Goals

- Add opt-in dynamic plugin support for `processor` and `exporter` nodes.
- Preserve static `linkme` registration as the default and first-class path.
- Keep phase 1 narrow enough to ship safely.
- Ensure plugin-backed nodes participate in config validation and live reconfiguration.
- Avoid exposing unstable engine internals directly across the plugin boundary.
- Keep the plugin ABI forward-compatible with a future Arrow-native payload path.

## Non-Goals

- Receiver plugins in phase 1.
- Extension plugins in phase 1.
- Remote plugin download or registry fetch.
- Plugin auto-update.
- In-place code unload/reload.
- Replacing built-in static nodes with plugins.
- Performance parity with native Arrow-based nodes in phase 1.

## Decision Summary

Phase 1 decisions:

- Backend: Wasmtime Component Model
- Scope: processors and exporters only
- ABI payload shape: variant supporting `otlp-proto-bytes` and reserved `otap-arrow-ipc`
- Implemented phase 1 payload: `otlp-proto-bytes` only
- Output semantics: single/default output only
- Runtime integration: host-owned adapter nodes
- Registry design: owned runtime registry overlay on top of static `PipelineFactory`
- Validation: plugin-exported validator mandatory; descriptor-embedded schema optional but recommended
- Runtime identity: plugin artifact fingerprint participates in rollout identity
- Processor execution policy: inline on per-core runtime with strict host-enforced deadline
- Exporter execution policy: bounded blocking worker pool
- Telemetry imports in v1: logs only
- Component cache in v1: disk-backed precompiled Wasmtime cache
- Integrity policy in v1 alpha: SHA-256 required; minisign supported
- Long-term security posture: signatures required by default before stable release

## Proposed Architecture

Dynamic plugins do not implement engine traits directly. Instead:

- the host discovers plugin manifests
- the host verifies and compiles Wasm components
- the host queries plugin descriptors
- the host synthesizes runtime processor/exporter entries
- those entries create host-owned adapter nodes that implement the existing processor/exporter runtime traits locally

New crates:

- `crates/plugin-api`: WIT world definitions, generated bindings, descriptor and compatibility types
- `crates/plugin-manifest`: manifest parsing, SHA-256 and minisign verification, compatibility checks
- `crates/plugin-host`: Wasmtime engine/linker/store management, compiled component cache, plugin descriptor loading, runtime registry construction
- `crates/plugin-nodes`: `WasmProcessorAdapter`, `WasmExporterAdapter`

Existing code changes:

- `crates/engine`: runtime registry abstraction layered above static `PipelineFactory`, dynamic validators
- `crates/controller`: accept an owned runtime registry handle
- `src/main.rs`: load plugins before config validation, build merged runtime registry, pass merged registry into startup/controller

## Required Code Refactors

### Runtime Registry Overlay

Introduce:

- `StaticComponentRegistry<PData>` backed by the current `PipelineFactory<PData>`
- `DynamicComponentRegistry<PData>` built from plugins
- `RuntimeComponentRegistry<PData>` that checks static first, dynamic second

This keeps static built-ins unchanged and localizes dynamic logic.

### Name Ownership

Static factories can remain backed by `&'static str`.
Dynamic entries require owned keys.

Recommendation:

- leave static factory maps structurally unchanged where possible
- runtime overlay uses owned keys such as `Arc<str>` or `String`

### Validator Abstraction

Introduce:

```rust
enum ConfigValidator {
    Static(fn(&serde_json::Value) -> Result<(), Error>),
    Dynamic(Arc<dyn Fn(&serde_json::Value) -> Result<(), Error> + Send + Sync>),
}
```

Dynamic validators call plugin `validate-config` using short-lived validation instances.

### Controller Ownership

The controller should accept:

- `Arc<RuntimeComponentRegistry<PData>>`

This must flow through startup validation and controller construction.

### Payload Conversion API

Expose a minimal public conversion surface from `crates/otap/src/pdata_conversions.rs` for plugin adapters:

- host payload -> OTLP bytes by signal
- OTLP bytes -> host payload by signal

### Runtime Identity Hook

Plugin identity must participate in live-control runtime comparison. The implementation must wire plugin fingerprint comparison into the runtime-shape comparison path used by live reconfiguration in `crates/controller/src/live_control`.

The fingerprint must include:

- component URN
- plugin version
- plugin artifact SHA-256
- plugin API version

## Plugin Manifest and ABI Proposal

### Manifest

```yaml
apiVersion: otap.plugin/v1alpha1
kind: WasmPlugin
metadata:
  name: acme-redact
  version: 0.1.0
runtime:
  kind: wasmtime-component
  path: ./acme_redact.wasm
  sha256: "..."
compatibility:
  pluginApi: "^0.1"
limits:
  memoryMaxBytes: 67108864
  epochDeadlineMs: 10
telemetry:
  logLevel: info
signing:
  minisignPublicKeyPath: ./trusted_plugins.pub
  # minisignSignaturePath: ./plugin.wasm.minisig  # optional; default: <artifact>.minisig
```

Manifest purpose:

- artifact location
- integrity policy
- execution limits
- compatibility declaration
- optional trust root/config

The manifest is not authoritative for component declarations. The plugin `descriptor()` export is authoritative.

### Descriptor Authority

The plugin exports a descriptor that defines:

- plugin name/version
- component URNs and kinds
- supported payload formats
- embedded JSON schema strings per component
- plugin API compatibility/version data

Any mismatch between manifest expectations and descriptor contents is a load failure.

### ABI Shape

Use WIT with a payload variant from day one:

- `otlp-proto-bytes`
- `otap-arrow-ipc`

Phase 1 host support:

- only `otlp-proto-bytes`
- `otap-arrow-ipc` is reserved and rejected as unsupported

Conceptual payload shape:

```wit
enum signal-type { logs, metrics, traces }

variant payload {
  otlp-proto-bytes(tuple<signal-type, list<u8>>),
  otap-arrow-ipc(tuple<signal-type, list<u8>>),
}
```

### Output Semantics

Processor plugins are single/default-output only in phase 1.

This is enforced at validation time and by the synthesized runtime wiring contract. Plugin processor configs are rejected if the node declares multiple outputs or if connections reference explicit non-default output selectors.

### Validation

The plugin exports:

- `descriptor()`
- `validate-config(component-urn, config-json)`

Rules:

- `descriptor()` is mandatory
- `validate-config()` is mandatory
- embedded JSON schema in the descriptor is optional but recommended

## Runtime Lifecycle

### Startup

1. resolve plugin directories and manifests
2. verify SHA-256
3. verify minisign signature if configured/enforced
4. load compiled component from disk cache or compile/precompile
5. call `descriptor()`
6. reject incompatible or unsupported components
7. construct dynamic registry entries
8. merge static and dynamic registries
9. run normal config validation against merged registry
10. start controller

### Instantiation

A plugin component is instantiated once per `(plugin, node, generation, core)` tuple.

- The compiled Wasm `Component` is cached globally.
- The `Store`/instance is per runtime node instance and lives on that core's execution context.

Operational note:

- memory floor is roughly `cores * plugin-nodes * memoryMaxBytes`
- operators must size `memoryMaxBytes` conservatively

### Steady State

Processor adapter:

- receives host payload
- converts to OTLP bytes
- calls plugin inline on the per-core runtime
- uses Wasmtime epoch interruption as the primary deadline mechanism
- may use fuel as a secondary per-call guard
- converts outputs back to host payloads
- emits only to default output

Exporter adapter:

- receives host payload
- converts to OTLP bytes
- dispatches plugin call via bounded blocking worker pool
- pool concurrency is capped by an explicit configuration knob
  (`PluginHostConfig::exporter_blocking_concurrency`, surfaced on the CLI
  as `--plugin-exporter-blocking-concurrency`, default 32). The cap is
  enforced by a shared `tokio::sync::Semaphore` owned by `PluginHost`
  and threaded into every `WasmExporterAdapter`; a permit is acquired
  before each `spawn_blocking` dispatch and released on completion.
- when the cap is saturated, dispatches **back-pressure** (await a
  permit) rather than failing fast; this preserves existing exporter
  no-data-loss semantics and naturally slows inbox drain. A closed
  semaphore is treated as a transient runtime error.
- does not allow concurrent calls into the same Wasm instance
- maps result back to exporter semantics

### Shutdown

- host calls plugin shutdown hook
- host drops instance/store after pipeline drain
- no attempt to unload compiled code globally in phase 1

## Live Reconfiguration Behavior

Plugin-backed nodes integrate with existing rollout semantics exactly as runtime shape changes.

Rules:

- plugin manifests/artifacts are validated before a candidate rollout is accepted
- plugin `validate-config` runs using a short-lived validation instance
- validation instances are separate from steady-state runtime instances
- runtime identity includes:
  - component URN
  - plugin version
  - plugin artifact SHA-256
  - plugin API version
- changing any of the above is treated as a pipeline change
- new generation gets fresh plugin instances
- old generation drains normally
- old plugin instances are dropped only after old generation completes

## Security Model

Default posture:

- no WASI capabilities by default
- no filesystem
- no network
- no env
- no unrestricted clock access
- memory limits enforced
- epoch interruption enforced
- artifact SHA-256 required always

Integrity/authenticity policy:

- phase 1 alpha: SHA-256 required, minisign supported
- unsigned plugin acceptance only via explicit dev override
- before stable release: signatures required by default

V1 signature scheme: minisign.

## Plugin Error Mapping

Plugin result classes must map deterministically to host semantics.

Recommended plugin error classes:

- `success`
- `retryable`
- `permanent`
- `fatal`

Mapping:

- `success` -> exporter ACK / processor success
- `retryable` -> retryable failure / NACK path where applicable
- `permanent` -> permanent drop/failure path without retry
- `fatal` -> node/runtime failure, eligible to fail candidate rollout or active instance

## Telemetry and Observability

Phase 1 plugin telemetry import:

- `telemetry.log(level, msg, kv)`

This is mandatory in v1. Metrics imports are deferred to phase 2.

## Testing and Benchmarking Plan

### Unit

- manifest parsing
- SHA/signature verification
- duplicate URN rejection
- compatibility rejection
- registry merge behavior
- dynamic validator behavior

### Integration

- `--validate-and-exit` with plugin-backed processor/exporter
- startup failure on missing plugin or bad hash
- plugin trap produces controlled failure
- rollout with plugin artifact change triggers replacement
- rollback on candidate plugin startup failure

### Golden

- native processor vs equivalent plugin processor on same OTLP input
- exporter success/retry/permanent/fatal mapping

### Benchmarks

- native processor/exporter baseline
- Wasm processor/exporter with OTLP bytes
- compare by signal and batch size

Phase 1 expectation must be stated clearly:

- plugin-backed processor/exporter paths are not for the hottest Arrow-native pipelines
- phase 2 Arrow IPC support is triggered by measured benchmark data

## Precompiled Component Cache

Disk-backed precompiled Wasmtime cache is part of v1.

Cache key should include:

- artifact SHA-256
- Wasmtime version
- target triple
- Wasmtime engine/compiler config fingerprint
- plugin API version

Suggested default location:

- `$XDG_CACHE_HOME/otap/plugins` or platform equivalent

## Alternatives Considered

### Raw `libloading`

Rejected as the primary path. It solves symbol loading, not Rust ABI safety.

### `abi_stable` as primary

Rejected for phase 1. Retained as a future trusted/internal fallback for native high-performance plugins.

### Out-of-process gRPC

Rejected for phase 1 due to operational and latency cost. Retained as a future fallback for stronger isolation or OS-heavy plugins.

### Extism

Not chosen as primary. Simpler, but less aligned than the Component Model for a typed long-lived processor/exporter ABI.

### Arrow IPC as phase 1 payload

Deferred, but reserved in the ABI now to avoid a future ABI break.

## Migration Plan

All steps below are landed on the `wasm` branch:

1. Runtime registry overlay with zero static behavior change. *(done)*
2. Validator abstraction (`ConfigValidator::Static` / `Dynamic`). *(done)*
3. Owned runtime registry threaded through startup/controller. *(done)*
4. Manifest loading + precompiled Wasmtime cache + minisign signature
   verification. *(done)*
5. `WasmProcessorAdapter` with epoch-deadline enforcement. *(done)*
6. `WasmExporterAdapter` with bounded blocking dispatch. *(done)*
7. Identity-plugin fixture + Wasmtime-backed integration tests. *(done)*
8. Feature-gating via the `wasmtime-backend` Cargo feature on
   `crates/plugin-host`. *(done)*

Static `linkme` nodes remain unchanged and fully supported throughout.

For an operator-facing walkthrough of the implemented surface, see
[`dynamic-plugins-usage.md`](./dynamic-plugins-usage.md).

## Open Questions

- What exact plugin fingerprint should be surfaced in status/debug/admin endpoints?
- What benchmark threshold should require phase 2 Arrow IPC work?
- Should manifest support explicit policy allowlists for specific URNs/components beyond descriptor authority?
