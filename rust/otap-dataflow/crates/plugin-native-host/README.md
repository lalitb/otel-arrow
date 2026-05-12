# `otap-df-plugin-native-host` — phase-1 native plugin host

Loads native cdylib plugins for `otap-dataflow` and dispatches them
through an opaque `OtapPdataHandle`.

## What this crate does

* Discovers `apiVersion: otap.plugin/v1alpha1`, `kind: NativePlugin`
  manifests under `--plugin-dir <DIR>` (multiple allowed). Manifests of
  other kinds are silently skipped (the wasm host scans the same
  directory).
* Verifies the cdylib SHA-256 (always required) and minisign signature
  (when configured).
* `dlopen`s the cdylib with `RTLD_LOCAL | RTLD_NOW` so plugins do not
  share C-runtime state and so missing symbols surface at load time.
* Looks up the versioned entry symbol `otap_plugin_register_v1`,
  validates the returned vtable: `abi_version` matches
  `OTAP_PLUGIN_ABI_VERSION_V1`, every required function-pointer slot
  is non-null, and every `_reserved` slot is zero. A vtable that fails
  any of these is rejected at load time with a clear error.
* Calls `descriptor()` and per-component `validate_config()` exports.
* Constructs per-node plugin instances on demand
  ([`PluginInstanceHandle`]), driven by the engine's static-first
  registry overlay via [`otap-df-plugin-native-nodes`].

## Phase-1 invariants

* Processor components only.
* Verbs: `ForwardSame`, `Drop`, `Error`.
* Phase 1 never explicitly unloads plugins. The library remains mapped
  while any loaded plugin, factory, or instance reference is alive.
  `df_engine` keeps loaded plugin handles alive in `main`'s scope for
  the lifetime of the process. Live reconfiguration replaces the
  *instance* but not the library mapping.

## Zero-copy claim — precise

* **`ForwardSame` forwards the original `OtapPdata` without payload
  serialization or reconstruction.** The integration test
  (`tests/native_plugin_e2e.rs`) asserts pointer equality on the inner
  `Bytes::as_ptr()` after the round-trip to prove this.
* **Inspection-time decoding is _not_ zero-copy.** Today the host
  resource-attribute accessor (`get_resource_attr_str`) lazily decodes
  the OTLP-bytes payload via `prost` into a per-call cache so the
  plugin can read attributes without itself parsing protobuf. The
  decoded buffers live only inside the host accessor for the duration
  of one `process` call and are never handed to the plugin as bytes.
* **Arrow `RecordBatch` / Arrow C Data Interface access is deferred to
  Phase 2.** OTAP Arrow records currently return `HOST_UNSUPPORTED`
  from the resource-attribute accessor; phase-2 will add zero-copy
  Arrow column accessors via the standard CDI.

## Trust and isolation — precise

Native plugins are **trusted code**, equivalent to code compiled into
the collector binary. There is **no isolation**:

* The host **does not** catch plugin panics. Plugin entry points are
  declared `extern "C"`; an uncaught panic crossing the boundary is
  undefined behavior and aborts the process under modern rustc.
* Plugins **MUST** ensure no panic crosses the ABI boundary — either
  by building with `panic = "abort"` (the recommended profile, used by
  the sample plugin) or by wrapping every exported function body in an
  in-plugin `catch_unwind` and converting any caught panic to
  `OtapPluginVerb::Error` (or a non-zero rc on `descriptor` /
  `validate_config`).
* The plugin **MUST NOT** share an `OtapPdataHandle` across threads
  or call host accessors with the same handle from more than one
  thread, even within a single `process` invocation. Host accessor
  state behind the handle is not internally synchronized.
* A plugin that corrupts memory, dereferences null, or invokes UB can
  corrupt the host or terminate the process. The host cannot recover
  from this. Operators should require signed plugins
  (`require_signed: true`) in production.

[`PluginInstanceHandle`]: ./src/runner.rs
[`otap-df-plugin-native-nodes`]: ../plugin-native-nodes
