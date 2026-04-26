# otap-df-plugin-host

Wasmtime-backed plugin host: descriptor + `validate-config` loading,
precompiled component cache, runner construction, and the shared
exporter blocking-pool semaphore.

The Wasmtime backend is feature-gated behind `wasmtime-backend`. With
the feature off, host APIs return `PluginError::BackendUnimplemented`
so misconfigurations surface explicitly.

See [`docs/dynamic-processor-exporter-plugins-phase1.md`](../../docs/dynamic-processor-exporter-plugins-phase1.md)
and the [operator usage guide](../../docs/dynamic-plugins-usage.md).
