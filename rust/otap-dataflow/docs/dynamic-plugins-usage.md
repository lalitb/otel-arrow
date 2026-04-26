# Dynamic Plugin Operator Guide (phase 1)

This is a short, operator-facing walkthrough of the dynamic plugin
support that ships in `otap-dataflow` phase 1. For the design rationale,
see [`dynamic-processor-exporter-plugins-phase1.md`](./dynamic-processor-exporter-plugins-phase1.md).

## What phase 1 supports

- **Processor and exporter** plugins, loaded as Wasmtime Component
  Model artifacts.
- A plugin contributes one or more components identified by URN
  (`urn:otel:<vendor>:<name>`); pipelines reference those URNs the
  same way they reference built-in static components.
- Static (`linkme`) components remain the default and take precedence
  over any plugin that declares the same URN.
- Phase-1 payload is **`otlp-proto-bytes`** only. `otap-arrow-ipc` is
  reserved in the WIT but rejected at load time.
- Single/default-output processors only.

What is **not** supported in phase 1: receiver plugins, extension
plugins, Arrow IPC execution, remote plugin fetch/auto-update, and
in-place reload of a running plugin (see "Live reconfiguration" below).

## Building the binary with plugin support

Plugin support is feature-gated to keep the default build small. Build
the binary with the `wasmtime-backend` feature on `plugin-host`:

```bash
cd rust/otap-dataflow
cargo build --release --features otap-df-plugin-host/wasmtime-backend
```

Without that feature, `--plugin-dir` will fail at startup with a clear
"wasmtime backend is not available in this build" error if any
manifest references a Wasmtime artifact.

## Manifest layout

Each plugin is described by a YAML manifest discovered at startup.
Minimum:

```yaml
apiVersion: otap.plugin/v1alpha1
kind: WasmPlugin
metadata:
  name: acme-redact
  version: 0.1.0
runtime:
  kind: wasmtime-component
  path: ./acme_redact.wasm
  sha256: "<hex-sha256-of-acme_redact.wasm>"
compatibility:
  pluginApi: "^0.1"
limits:
  memoryMaxBytes: 67108864
  epochDeadlineMs: 10
signing:
  # Optional. When set, the artifact is verified against this key
  # regardless of the host `--plugin-require-signed` flag.
  minisignPublicKeyPath: ./trusted_plugins.pub
  # Optional. Defaults to `<runtime.path>.minisig`.
  # minisignSignaturePath: ./acme_redact.wasm.minisig
```

`runtime.path` and `runtime.sha256` are mandatory. The SHA-256 is
verified against the artifact bytes on every load.

The plugin's exported `descriptor()` (not the manifest) is
authoritative for which component URNs/kinds the plugin contributes.

## CLI flags

The following flags are surfaced by `df_engine` (`src/main.rs`):

| Flag | Default | Description |
|---|---|---|
| `--plugin-dir <DIR>` | none | Directory of plugin manifests (`*.yaml`). Repeatable. |
| `--plugin-cache-dir <DIR>` | platform cache | Disk cache for precompiled Wasmtime components. |
| `--plugin-require-signed` | off | Reject any plugin without a verified minisign signature. |
| `--plugin-exporter-blocking-concurrency <N>` | `32` | Cross-instance cap on concurrent exporter `spawn_blocking` dispatches. |

Pass `--plugin-dir` once per directory:

```bash
df_engine \
  --plugin-dir /etc/otap/plugins \
  --plugin-dir ./local-plugins \
  --plugin-cache-dir /var/cache/otap/plugins \
  --plugin-require-signed \
  --config pipeline.yaml
```

## Signature policy

- **SHA-256 integrity** is mandatory for every plugin and verified on
  every load.
- **Minisign signatures** use this matrix:

  | manifest key path | `--plugin-require-signed` | result |
  | --- | --- | --- |
  | absent  | absent | unsigned plugin accepted (alpha policy) |
  | absent  | set    | plugin rejected |
  | present | either | signature verified; reject on mismatch or missing `.minisig` |

- A signature on disk that fails verification is **always** rejected,
  regardless of the host flag.

## Runtime behavior

- **Processor plugins** are invoked inline on the per-core runtime
  with a Wasmtime epoch-interruption deadline (`epochDeadlineMs`).
  The `OtapPdata` context (transport headers, ack/nack lineage, etc.)
  is preserved across the plugin call; only the payload bytes are
  transformed.
- **Exporter plugins** are dispatched through `tokio::spawn_blocking`,
  governed by a single host-wide `Semaphore` whose size is set by
  `--plugin-exporter-blocking-concurrency`. When the cap saturates,
  dispatches **back-pressure** (await a permit) rather than fail
  fast. Per-instance serialization is preserved automatically by the
  engine (sequential `Box<Self>` dispatch).

## Live reconfiguration

Plugin fingerprints participate in rollout planning. The fingerprint
includes:

- component URN
- plugin version
- artifact SHA-256
- plugin API version

A change to any fingerprint of any component referenced by a pipeline
forces `RolloutAction::Replace` (full restart of that pipeline). A
core-count change with unchanged fingerprints produces
`RolloutAction::Resize`. No-change replans produce
`RolloutAction::NoOp`. There is no in-place "swap the .wasm" reload;
upgrades go through rollout.

## Diagnostics

Common load failures and what they mean:

- `wasmtime backend is not available in this build` — rebuild with
  `--features otap-df-plugin-host/wasmtime-backend`.
- `artifact integrity check failed` — manifest `runtime.sha256` does
  not match the bytes at `runtime.path`.
- `signature verification failed` — minisign verification failed; the
  plugin is rejected regardless of `--plugin-require-signed`.
- `plugin requires unsupported payload format: otap-arrow-ipc` — the
  plugin descriptor declares only Arrow IPC, which is reserved for
  phase 2.
- `duplicate component URN` — two plugins declare the same URN, or a
  plugin declares a URN that is already registered statically (static
  takes precedence; the duplicate plugin entry is rejected).

## Limitations summary (phase 1)

- OTLP-bytes payload only.
- Single/default-output processors only.
- No receiver or extension plugins.
- No Arrow IPC execution (reserved in WIT).
- No remote fetch / auto-update.
- No in-place reload — upgrades flow through rollout.
- Logs-only telemetry import for plugins (metrics deferred).
