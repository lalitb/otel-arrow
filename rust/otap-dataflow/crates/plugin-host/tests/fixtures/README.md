# Plugin host test fixtures

## `identity_plugin.wat`

A minimal Wasm component that implements the `otap-plugin` world. The
fixture is plain WebAssembly Text (WAT); the integration test compiles
it on the fly with the [`wat`] crate, so no out-of-tree binary
artifacts are committed.

Exports:

- `descriptor() -> string` — returns a fixed JSON `PluginDescriptor`
  declaring a single processor component
  (`urn:otel:test:identity`) supporting `otlp-proto-bytes` only.
- `validate-config(string) -> result<_, string>` — accepts any config
  (always returns `Ok`).
- `process(signal: u32, payload-kind: u32, payload: list<u8>, config: string)
  -> result<tuple<u32, list<u8>>, string>` — identity transform:
  echoes `(signal, payload)` back with class `0` (OK).

The fixture is intentionally written in raw component-model WAT so it
has no host-side build dependencies (no `cargo component`, no
`wit-bindgen`). It is the minimum surface needed to prove the
phase-1 descriptor / validate / process path through the real Wasmtime
backend end-to-end.

[`wat`]: https://crates.io/crates/wat
