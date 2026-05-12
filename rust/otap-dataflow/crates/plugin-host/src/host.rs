// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Top-level plugin host: discovery, verification, descriptor loading.

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;

use otap_df_plugin_api::{PLUGIN_API_VERSION, PluginDescriptor, PluginError, PluginFingerprint};
use otap_df_plugin_manifest::{
    LoadedManifest, RuntimeSpec, load_manifest, verify_artifact_sha256, verify_artifact_signature,
};
use tokio::sync::Semaphore;

use crate::cache::{ComponentCacheKey, target_triple};
use crate::descriptor_loader::verify_phase1_descriptor;
use crate::runner::PluginRunner;
#[cfg(not(feature = "wasmtime-backend"))]
use crate::runner::UnimplementedRunner;
use crate::validator::PluginConfigValidator;
#[cfg(not(feature = "wasmtime-backend"))]
use crate::validator::UnimplementedValidator;

/// Phase-1 default concurrency cap for plugin exporter blocking-pool
/// dispatch. Conservative; raise via [`PluginHostConfig`] when needed.
///
/// Rationale: most phase-1 deployments will run a small number of
/// plugin exporters; 32 in-flight blocking calls is enough headroom to
/// keep batchers busy without letting a slow plugin saturate Tokio's
/// blocking pool (default 512) and starve unrelated work like file
/// I/O and DNS resolution.
pub const DEFAULT_EXPORTER_BLOCKING_CONCURRENCY: usize = 32;

/// Configuration for a plugin host instance.
#[derive(Clone, Debug)]
pub struct PluginHostConfig {
    /// Directories the host scans for `*.yaml` manifests.
    pub plugin_dirs: Vec<PathBuf>,
    /// On-disk precompiled component cache directory. When `None`, caching
    /// is disabled (phase-1 default for tests; production should set this
    /// to `$XDG_CACHE_HOME/otap/plugins`).
    pub cache_dir: Option<PathBuf>,
    /// When `true`, the host rejects unsigned plugins. Phase-1 alpha
    /// default is `false` with a deprecation warning; this becomes `true`
    /// before stable release.
    pub require_signed: bool,
    /// Maximum number of concurrent in-flight plugin exporter blocking
    /// tasks across **all** dynamic exporter instances.
    ///
    /// Backing mechanism is a [`tokio::sync::Semaphore`] whose permits are
    /// acquired before each `spawn_blocking` dispatch in
    /// `WasmExporterAdapter::start`. Saturation behavior is *back-pressure*:
    /// the exporter `await`s a permit, which naturally slows draining of
    /// the exporter inbox without dropping data.
    ///
    /// Per-instance serialization (an exporter is never called
    /// concurrently with itself) is unaffected — that property comes from
    /// `Box<Self>` ownership in `Exporter::start`.
    ///
    /// Defaults to [`DEFAULT_EXPORTER_BLOCKING_CONCURRENCY`].
    pub exporter_blocking_concurrency: NonZeroUsize,
}

impl Default for PluginHostConfig {
    fn default() -> Self {
        Self {
            plugin_dirs: Vec::new(),
            cache_dir: None,
            require_signed: false,
            exporter_blocking_concurrency: NonZeroUsize::new(DEFAULT_EXPORTER_BLOCKING_CONCURRENCY)
                .expect("DEFAULT_EXPORTER_BLOCKING_CONCURRENCY > 0"),
        }
    }
}

/// One successfully loaded plugin. Returned by [`PluginHost::load_all`] for
/// downstream crates to translate into engine registry entries.
pub struct LoadedPlugin {
    /// The manifest as parsed and resolved against disk.
    pub manifest: LoadedManifest,
    /// The plugin descriptor returned by `descriptor()`.
    pub descriptor: PluginDescriptor,
    /// Per-component fingerprints derived from manifest + descriptor.
    pub fingerprints: Vec<PluginFingerprint>,
    /// Per-component config validators.
    ///
    /// One entry per component URN. The validator backs the engine's
    /// `ConfigValidator::Dynamic` and forwards into the plugin's
    /// `validate-config` export. With the `wasmtime-backend` feature
    /// disabled this is an [`UnimplementedValidator`] that fails closed.
    pub validators: Vec<(String, Arc<dyn PluginConfigValidator>)>,
    /// Per-component runtime runners.
    ///
    /// Backs the dynamic processor/exporter factories produced by
    /// `plugin-nodes`. With `wasmtime-backend` disabled this is an
    /// [`UnimplementedRunner`] so factory invocation fails clearly at
    /// pipeline-build time rather than at first message.
    pub runners: Vec<(String, Arc<dyn PluginRunner>)>,
    /// Cache key for the precompiled component.
    pub cache_key: ComponentCacheKey,
}

/// Plugin host.
pub struct PluginHost {
    config: PluginHostConfig,
    /// Shared semaphore that caps the number of concurrent in-flight
    /// plugin exporter blocking tasks across all dynamic exporter
    /// instances. Sized from [`PluginHostConfig::exporter_blocking_concurrency`].
    exporter_blocking_permits: Arc<Semaphore>,
}

impl PluginHost {
    /// Construct a new host with the given config.
    #[must_use]
    pub fn new(config: PluginHostConfig) -> Self {
        let permits = Arc::new(Semaphore::new(config.exporter_blocking_concurrency.get()));
        Self {
            config,
            exporter_blocking_permits: permits,
        }
    }

    /// Convenience: a host with no plugin directories. Always loads zero
    /// plugins. Distributions that opt out of dynamic plugins use this.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(PluginHostConfig::default())
    }

    /// Borrow the host configuration.
    #[must_use]
    pub fn config(&self) -> &PluginHostConfig {
        &self.config
    }

    /// Shared semaphore that bounds concurrent plugin exporter blocking
    /// dispatch. Used by `WasmExporterAdapter` (via `plugin-nodes`) to
    /// acquire a permit before each `spawn_blocking` call.
    ///
    /// Cloning is cheap (`Arc::clone`).
    #[must_use]
    pub fn exporter_blocking_permits(&self) -> Arc<Semaphore> {
        Arc::clone(&self.exporter_blocking_permits)
    }

    /// Discover and load every plugin in the configured directories.
    pub fn load_all(&self) -> Result<Vec<LoadedPlugin>, PluginError> {
        if self.config.plugin_dirs.is_empty() {
            return Ok(Vec::new());
        }

        let mut loaded = Vec::new();
        for dir in &self.config.plugin_dirs {
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_file()
                    && path
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e == "yaml" || e == "yml")
                        .unwrap_or(false)
                {
                    // Pre-parse the manifest to skip artifacts owned by
                    // a different backend (e.g. native cdylib plugins).
                    // The native plugin host scans the same `--plugin-dir`
                    // and loads those itself.
                    let parsed = load_manifest(&path)?;
                    if !matches!(
                        &parsed.manifest.runtime,
                        RuntimeSpec::WasmtimeComponent { .. }
                    ) {
                        continue;
                    }
                    let l = self.load_one(&path)?;
                    loaded.push(l);
                }
            }
        }
        Ok(loaded)
    }

    /// Load a single manifest path.
    pub fn load_one(&self, manifest_path: &std::path::Path) -> Result<LoadedPlugin, PluginError> {
        let manifest = load_manifest(manifest_path)?;
        // Wasm host only handles WasmtimeComponent artifacts. Native cdylib
        // manifests are routed through `otap-df-plugin-native-host`.
        if !matches!(
            &manifest.manifest.runtime,
            RuntimeSpec::WasmtimeComponent { .. }
        ) {
            return Err(PluginError::ManifestParse(format!(
                "wasm plugin host received non-wasm manifest: {}",
                manifest_path.display()
            )));
        }
        verify_artifact_sha256(&manifest)?;
        verify_artifact_signature(&manifest, self.config.require_signed)?;

        // With the wasmtime-backend feature on, reuse the same compiled
        // component for descriptor loading, validator construction, and
        // runner construction so we don't pay the JIT cost three times.
        // Without the feature, fall back to the public `load_descriptor`
        // (which returns `BackendUnimplemented`) and stub validators+runners.
        #[cfg(feature = "wasmtime-backend")]
        let (descriptor, validator_factory, runner_factory): (
            PluginDescriptor,
            Box<dyn Fn() -> Arc<dyn PluginConfigValidator>>,
            Box<dyn Fn() -> Arc<dyn PluginRunner>>,
        ) = {
            let loaded = crate::wasmtime_backend::load_component(&manifest.artifact_path)?;
            let descriptor = crate::wasmtime_backend::call_descriptor(&loaded)?;
            let loaded_for_validator = loaded.clone();
            let validator_factory: Box<dyn Fn() -> Arc<dyn PluginConfigValidator>> =
                Box::new(move || crate::wasmtime_backend::make_validator(&loaded_for_validator));
            let runner_factory: Box<dyn Fn() -> Arc<dyn PluginRunner>> =
                Box::new(move || crate::wasmtime_backend::make_runner(&loaded));
            (descriptor, validator_factory, runner_factory)
        };

        #[cfg(not(feature = "wasmtime-backend"))]
        let (descriptor, validator_factory, runner_factory): (
            PluginDescriptor,
            Box<dyn Fn() -> Arc<dyn PluginConfigValidator>>,
            Box<dyn Fn() -> Arc<dyn PluginRunner>>,
        ) = {
            let descriptor = crate::descriptor_loader::load_descriptor(&manifest.artifact_path)?;
            let validator_factory: Box<dyn Fn() -> Arc<dyn PluginConfigValidator>> =
                Box::new(|| Arc::new(UnimplementedValidator) as Arc<dyn PluginConfigValidator>);
            let runner_factory: Box<dyn Fn() -> Arc<dyn PluginRunner>> =
                Box::new(|| Arc::new(UnimplementedRunner) as Arc<dyn PluginRunner>);
            (descriptor, validator_factory, runner_factory)
        };

        verify_phase1_descriptor(&descriptor)?;

        let sha = match &manifest.manifest.runtime {
            RuntimeSpec::WasmtimeComponent { sha256, .. } => sha256.to_lowercase(),
            // Unreachable: native manifests are filtered above.
            RuntimeSpec::NativeCdylib { .. } => unreachable!("guarded above"),
        };

        let cache_key = ComponentCacheKey {
            artifact_sha256: sha.clone(),
            wasmtime_version: wasmtime_version_string(),
            engine_config_fingerprint: engine_config_fingerprint_string(),
            target_triple: target_triple().to_string(),
            plugin_api_version: PLUGIN_API_VERSION,
        };

        let fingerprints: Vec<PluginFingerprint> = descriptor
            .components
            .iter()
            .map(|c| PluginFingerprint {
                component_urn: c.urn.clone(),
                plugin_version: manifest.manifest.metadata.version.clone(),
                artifact_sha256: sha.clone(),
                plugin_api_version: descriptor.plugin_api_version,
            })
            .collect();

        let validators: Vec<(String, Arc<dyn PluginConfigValidator>)> = descriptor
            .components
            .iter()
            .map(|c| (c.urn.clone(), validator_factory()))
            .collect();

        let runners: Vec<(String, Arc<dyn PluginRunner>)> = descriptor
            .components
            .iter()
            .map(|c| (c.urn.clone(), runner_factory()))
            .collect();

        Ok(LoadedPlugin {
            manifest,
            descriptor,
            fingerprints,
            validators,
            runners,
            cache_key,
        })
    }
}

/// Wasmtime version string used in cache keys. With the backend disabled
/// this is the "wasmtime-unwired" sentinel so caches built without the
/// runtime can never be confused with real ones.
fn wasmtime_version_string() -> String {
    #[cfg(feature = "wasmtime-backend")]
    {
        crate::wasmtime_backend::wasmtime_version().to_string()
    }
    #[cfg(not(feature = "wasmtime-backend"))]
    {
        "wasmtime-unwired".to_string()
    }
}

/// Engine/compiler config fingerprint used in cache keys. Without the
/// backend, falls back to the "engine-unwired" sentinel.
fn engine_config_fingerprint_string() -> String {
    #[cfg(feature = "wasmtime-backend")]
    {
        crate::wasmtime_backend::engine_config_fingerprint()
    }
    #[cfg(not(feature = "wasmtime-backend"))]
    {
        "engine-unwired".to_string()
    }
}
