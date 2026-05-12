# `otap-df-plugin-native-nodes` — phase-1 adapter

Implements the engine's `local::Processor<OtapPdata>` for native cdylib
plugins. The adapter passes an opaque `OtapPdataHandle` to the plugin.

## Zero-copy claim — precise

`ForwardSame` forwards the original `OtapPdata` without payload
serialization or reconstruction. The integration test in
`tests/native_plugin_e2e.rs` asserts pointer equality on the inner
`Bytes::as_ptr()` after the round-trip.

This is **forwarding zero-copy**, not inspection zero-copy. Plugins
that read attributes through the host vtable trigger lazy `prost`
decoding inside the host accessor (see
`crates/plugin-native-host/src/handle.rs`). Arrow CDI inspection is
deferred to Phase 2.

## Registry bridge

* `build_native_registry(&[LoadedNativePlugin])` — fresh registry
  with one entry per declared component.
* `extend_native_registry(&mut DynamicComponentRegistry<OtapPdata>, &[LoadedNativePlugin])`
  — merges native entries into an existing dynamic registry. `df_engine`'s
  `main.rs` uses this to produce a single overlay shared by both the
  wasm and native backends.

The `fingerprint` field of each `DynamicProcessorEntry` carries the
plugin's `(component_urn, plugin_version, artifact_sha256, plugin_api_version)`
tuple. `crates/controller/src/live_control/planning.rs::collect_plugin_fingerprints`
snapshots this vector per-pipeline and compares it across rollouts;
any difference triggers `RolloutAction::Replace`. The test
`fingerprint_change_triggers_replace_via_planning_equality` in
`tests/native_plugin_e2e.rs` pins the *comparison primitive* (the
`DynamicNodeFingerprint` equality the planner builds vectors from);
full `RolloutAction::Replace` integration is a follow-up.

## Sample plugin

`tests/fixtures/drop_logs_by_service/` — cdylib with `panic = "abort"`
that drops logs whose `service.name` resource attribute matches a
configured value. See its `Cargo.toml` for plugin profile settings.
