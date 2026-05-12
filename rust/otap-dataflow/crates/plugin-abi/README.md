# `otap-df-plugin-abi` — stable C ABI for native plugins (phase 1)

Defines the `#[repr(C)]` types crossed between the otap-dataflow
native plugin host and cdylib plugins.

* `OTAP_PLUGIN_ABI_VERSION_V1 = 1` — bumped only on breaking layout
  changes.
* Versioned plugin entry symbol: `otap_plugin_register_v1`.
* Verbs: `ForwardSame`, `Drop`, `Error` — zero-copy on the host side
  (`ForwardSame` forwards the original `OtapPdata`).
* Opaque `OtapPdataHandle` + a host-supplied accessor vtable
  (`OtapHostVTable`). Plugins never receive serialized payload bytes.
* Reserved slots for additive minor-version growth.

See `crates/plugin-native-host` for the host implementation and
`crates/plugin-native-nodes/tests/fixtures/drop_logs_by_service` for a
sample plugin.

## Why plain C ABI (not abi_stable / stabby)

The phase-1 surface is small (one register fn, ~5 vtable entries, one
host accessor at first). A plain `#[repr(C)]` boundary keeps the door
open for non-Rust plugins later (the same approach used by Arrow C
Data Interface, DuckDB extensions, Postgres `PG_MODULE_MAGIC`).
