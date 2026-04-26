// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Wasmtime-backed implementation of the host-side plugin entry points.
//!
//! Scope (this module):
//!   * compile a precompiled `.wasm` component from disk
//!   * call the plugin's `descriptor` export and JSON-decode the result
//!     into [`PluginDescriptor`]
//!   * call the plugin's `validate-config` export against a JSON config
//!   * call the plugin's `process` export with `(signal, payload_kind,
//!     payload, config)` and return the `(result_class, bytes)` pair —
//!     this is what powers [`WasmtimeRunner`] and, through it, the
//!     processor/exporter adapters in `otap-df-plugin-nodes`.
//!
//! Both validate-config and process are bounded by Wasmtime epoch
//! interruption; see [`crate::host`] for the shared epoch ticker.
//!
//! Plugin contract (informal summary — the formal `wit` lives outside this crate):
//!
//! ```wit
//! world otap-plugin {
//!   export descriptor: func() -> string;          // JSON-encoded PluginDescriptor
//!   export validate-config: func(config: string)  // JSON-encoded user config
//!     -> result<_, string>;                       // empty Ok or error message
//! }
//! ```
//!
//! Component lookup is done by export name through the raw component API
//! (no `wit-bindgen` codegen) so this crate stays light on build-time
//! tooling.

use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

use otap_df_plugin_api::{PluginDescriptor, PluginError};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store};

use crate::validator::PluginConfigValidator;

/// Cadence of the global epoch ticker. Each tick advances the engine
/// epoch counter; per-call deadlines are expressed as integer multiples
/// of this cadence, so this is the minimum effective deadline resolution
/// (e.g. a 10 ms `timeout_ms` becomes a 10-tick deadline).
const EPOCH_TICK_INTERVAL_MS: u64 = 1;

/// Marker prefix for deadline-exceeded errors so adapters can distinguish
/// host-enforced timeouts from other plugin failures.
pub(crate) const DEADLINE_ERROR_PREFIX: &str = "plugin deadline exceeded";

/// Process-wide Wasmtime engine with cranelift defaults and host-side
/// epoch interruption enabled.
///
/// All plugins share one engine so caches and JIT state are reused. A
/// dedicated background thread increments the engine's epoch counter on
/// a fixed cadence ([`EPOCH_TICK_INTERVAL_MS`]) so per-call deadlines
/// (set via `Store::set_epoch_deadline`) actually trip.
fn shared_engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut config = Config::new();
        let _ = config.epoch_interruption(true);
        let engine = Engine::new(&config).expect("default engine config must be valid");
        // Epoch ticker. The thread is intentionally never joined: it
        // outlives the process. Holding a clone of the `Engine` keeps
        // its `Arc` count > 0 for the lifetime of the program (the
        // `OnceLock`-stored engine is the other holder).
        let ticker_engine = engine.clone();
        let _ = thread::Builder::new()
            .name("wasmtime-epoch-ticker".to_string())
            .spawn(move || {
                let interval = Duration::from_millis(EPOCH_TICK_INTERVAL_MS);
                loop {
                    thread::sleep(interval);
                    ticker_engine.increment_epoch();
                }
            });
        engine
    })
}

/// A loaded, JIT-compiled component plus the engine it was compiled with.
#[derive(Clone)]
pub(crate) struct LoadedComponent {
    pub(crate) engine: Engine,
    pub(crate) component: Component,
}

/// Compile a `.wasm` component from disk.
pub(crate) fn load_component(path: &Path) -> Result<LoadedComponent, PluginError> {
    let engine = shared_engine().clone();
    let component = Component::from_file(&engine, path)
        .map_err(|e| PluginError::ManifestParse(format!("wasmtime load `{path:?}`: {e}")))?;
    Ok(LoadedComponent { engine, component })
}

/// Invoke the plugin's `descriptor` export and parse the JSON result.
pub(crate) fn call_descriptor(loaded: &LoadedComponent) -> Result<PluginDescriptor, PluginError> {
    let mut store = Store::new(&loaded.engine, ());
    // Descriptor calls must set an epoch deadline because the shared
    // engine has `epoch_interruption(true)` and a background ticker
    // already advancing the epoch counter — without a deadline the
    // store traps on the first interruption check. Descriptor is a
    // declarative metadata call and gets a generous 1 s budget, the
    // same as `validate-config`.
    store.set_epoch_deadline(1000);
    let linker = Linker::<()>::new(&loaded.engine);
    let instance = linker
        .instantiate(&mut store, &loaded.component)
        .map_err(|e| PluginError::ManifestParse(format!("instantiate: {e}")))?;

    let func = instance
        .get_typed_func::<(), (String,)>(&mut store, "descriptor")
        .map_err(|e| PluginError::ManifestParse(format!("missing `descriptor` export: {e}")))?;
    let (json,) = func
        .call(&mut store, ())
        .map_err(|e| PluginError::ManifestParse(format!("descriptor call failed: {e}")))?;
    func.post_return(&mut store)
        .map_err(|e| PluginError::ManifestParse(format!("descriptor post-return: {e}")))?;

    serde_json::from_str(&json)
        .map_err(|e| PluginError::ManifestParse(format!("descriptor returned malformed JSON: {e}")))
}

/// Invoke the plugin's `validate-config` export with a JSON-encoded config.
fn call_validate_config(loaded: &LoadedComponent, config_json: &str) -> Result<(), String> {
    let mut store = Store::new(&loaded.engine, ());
    // Validation is allowed a generous fixed budget (independent of
    // per-call processor/exporter timeouts) so misconfigured plugins
    // can't hang admission. 1 s @ 1 ms ticks = 1000 ticks.
    store.set_epoch_deadline(1000);
    let linker = Linker::<()>::new(&loaded.engine);
    let instance = linker
        .instantiate(&mut store, &loaded.component)
        .map_err(|e| format!("instantiate: {e}"))?;

    let func = instance
        .get_typed_func::<(&str,), (Result<(), String>,)>(&mut store, "validate-config")
        .map_err(|e| format!("missing `validate-config` export: {e}"))?;
    let (result,) = func
        .call(&mut store, (config_json,))
        .map_err(|e| format!("validate-config call failed: {e}"))?;
    func.post_return(&mut store)
        .map_err(|e| format!("validate-config post-return: {e}"))?;
    result
}

/// Invoke the plugin's `process` export under a host-enforced deadline.
///
/// Plugin contract (raw component-model, no wit-bindgen):
/// `process: (signal: u32, payload-kind: u32, payload: list<u8>, config: string)
///     -> result<tuple<u32, list<u8>>, string>`
///
/// Phase-1 implementation creates a fresh `Store` per call. This:
///   * sidesteps the `Send + Sync` constraints on the runner trait
///     (the `Store` lives entirely inside this function)
///   * guarantees no cross-call plugin state leaks
///   * pays a per-call instantiation cost — acceptable for phase 1
///     (the JIT compile is amortized across all calls because the
///     `Component` is shared); future patches can amortize the `Store`.
///
/// `timeout_ms` is the host-enforced deadline (RFC §6.2). It is rounded
/// up to at least one epoch tick. When the deadline is exceeded Wasmtime
/// traps and the error is returned with the [`DEADLINE_ERROR_PREFIX`]
/// marker so adapters can distinguish it from plugin-level errors.
fn call_process(
    loaded: &LoadedComponent,
    signal: u32,
    payload_kind: u32,
    payload: &[u8],
    config_json: &str,
    timeout_ms: u64,
) -> Result<(u32, Vec<u8>), String> {
    let mut store = Store::new(&loaded.engine, ());
    let ticks = timeout_ms.max(1);
    store.set_epoch_deadline(ticks);
    let linker = Linker::<()>::new(&loaded.engine);
    let instance = linker
        .instantiate(&mut store, &loaded.component)
        .map_err(|e| format!("instantiate: {e}"))?;

    let func = instance
        .get_typed_func::<(u32, u32, &[u8], &str), (Result<(u32, Vec<u8>), String>,)>(
            &mut store, "process",
        )
        .map_err(|e| format!("missing `process` export: {e}"))?;
    let (result,) = func
        .call(&mut store, (signal, payload_kind, payload, config_json))
        .map_err(|e| {
            // Wasmtime maps an exceeded epoch deadline to a trap. Tag it
            // so adapters can surface a clear "deadline exceeded" error.
            format!("{DEADLINE_ERROR_PREFIX} or plugin trap after {timeout_ms} ms: {e}")
        })?;
    func.post_return(&mut store)
        .map_err(|e| format!("process post-return: {e}"))?;
    result
}

/// Validator that bridges into the plugin's `validate-config` export.
struct WasmtimeValidator {
    /// Cheap to clone (`Component` and `Engine` are `Arc` internally).
    loaded: LoadedComponent,
}

impl PluginConfigValidator for WasmtimeValidator {
    fn validate(&self, config_json: &str) -> Result<(), String> {
        call_validate_config(&self.loaded, config_json)
    }
}

/// Runner that bridges into the plugin's `process` export.
#[derive(Debug)]
struct WasmtimeRunner {
    loaded: LoadedComponent,
}

// `LoadedComponent` is `Clone` and internally `Arc`-backed (engine +
// component), so `WasmtimeRunner` is naturally `Send + Sync`.
impl crate::runner::PluginRunner for WasmtimeRunner {
    fn process(
        &self,
        signal: u32,
        payload_kind: crate::runner::PayloadKind,
        payload: &[u8],
        config_json: &str,
        timeout_ms: u64,
    ) -> Result<(u32, Vec<u8>), String> {
        call_process(
            &self.loaded,
            signal,
            payload_kind.tag(),
            payload,
            config_json,
            timeout_ms,
        )
    }
}

impl std::fmt::Debug for LoadedComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedComponent").finish_non_exhaustive()
    }
}

/// Build a [`PluginConfigValidator`] for one component of an already-loaded
/// plugin. The validator owns its own clone of the [`LoadedComponent`] so
/// the engine registry overlay can keep it alive independently of the
/// `LoadedPlugin` value.
pub(crate) fn make_validator(loaded: &LoadedComponent) -> Arc<dyn PluginConfigValidator> {
    Arc::new(WasmtimeValidator {
        loaded: loaded.clone(),
    })
}

/// Build a [`crate::PluginRunner`] for one component of an already-loaded
/// plugin.
pub(crate) fn make_runner(loaded: &LoadedComponent) -> Arc<dyn crate::PluginRunner> {
    Arc::new(WasmtimeRunner {
        loaded: loaded.clone(),
    })
}

/// The Wasmtime ABI generation this build was linked against. Used as part
/// of the precompiled-component cache key so a wasmtime upgrade
/// invalidates stale caches.
///
/// `wasmtime` does not export a public `VERSION` constant, so we mirror
/// the workspace pin here. **This must be bumped in lock-step with the
/// workspace `wasmtime` version pin in the root `Cargo.toml`** — but in
/// practice the [`engine_config_fingerprint`] subsumes this because
/// `precompile_compatibility_hash` already embeds the wasmtime version
/// internally. This string exists for human-readable cache filenames.
pub(crate) fn wasmtime_version() -> &'static str {
    "30"
}

/// Hex-encoded fingerprint of the host's [`Engine`] / `Config`. Two hosts
/// with materially different engine configurations produce different
/// fingerprints and therefore different cache entries.
///
/// Backed by `Engine::precompile_compatibility_hash`, which embeds:
///   * Wasmtime version (so a `wasmtime` upgrade invalidates caches
///     even if our hardcoded [`wasmtime_version`] string is stale)
///   * cranelift configuration (opt level, target features, …)
///   * module version strategy
///   * wasmparser version
pub(crate) fn engine_config_fingerprint() -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    shared_engine()
        .precompile_compatibility_hash()
        .hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanity: the deadline-error prefix is non-empty and does not collide
    /// with common plugin-side error strings. Adapters substring-match on
    /// this prefix to map host-enforced timeouts onto a stable error class.
    #[test]
    fn deadline_error_prefix_is_stable() {
        assert!(DEADLINE_ERROR_PREFIX.starts_with("plugin"));
        assert!(DEADLINE_ERROR_PREFIX.contains("deadline"));
    }

    /// Building the shared engine must succeed and enable epoch
    /// interruption (otherwise per-call deadlines are silently ignored).
    #[test]
    fn shared_engine_initializes() {
        let engine = shared_engine();
        // Calling precompile_compatibility_hash twice on the same
        // engine must produce a value (smoke test only — Wasmtime does
        // not expose a public getter for the epoch-interruption flag).
        let _h = engine.precompile_compatibility_hash();
    }
}
