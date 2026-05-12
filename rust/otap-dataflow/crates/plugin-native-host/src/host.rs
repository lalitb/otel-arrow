// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Native plugin host: discovery, integrity verification, descriptor
//! loading, validator/runner construction.

use std::path::PathBuf;
use std::sync::Arc;

use libloading::Library;
#[cfg(unix)]
use libloading::os::unix::{Library as UnixLibrary, RTLD_LOCAL, RTLD_NOW};

use otap_df_plugin_abi::{
    OTAP_PLUGIN_ABI_VERSION_V1, OtapPluginDescriptorRaw, OtapPluginRegisterV1, OtapPluginVTable,
    REGISTER_SYMBOL_V1,
};
use otap_df_plugin_api::{
    ComponentDescriptor, PLUGIN_API_VERSION, PluginApiVersion, PluginDescriptor, PluginError,
    PluginFingerprint,
};
use otap_df_plugin_manifest::{
    LoadedManifest, RuntimeSpec, load_manifest, verify_artifact_sha256, verify_artifact_signature,
};

use crate::runner::{
    NativePluginConfigValidator, NativeProcessorRunnerImpl, PluginInstanceHandle,
    SharedPluginLibrary, validate_vtable,
};

/// Best-effort target triple of the running host. Mirrors the helper in
/// `otap-df-plugin-host::cache` so the cache key shape can match.
#[must_use]
pub fn target_triple() -> &'static str {
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    {
        "x86_64-unknown-linux-gnu"
    }
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    {
        "aarch64-unknown-linux-gnu"
    }
    #[cfg(all(target_arch = "x86_64", target_os = "macos"))]
    {
        "x86_64-apple-darwin"
    }
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    {
        "aarch64-apple-darwin"
    }
    #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
    {
        "x86_64-pc-windows-msvc"
    }
    #[cfg(not(any(
        all(target_arch = "x86_64", target_os = "linux"),
        all(target_arch = "aarch64", target_os = "linux"),
        all(target_arch = "x86_64", target_os = "macos"),
        all(target_arch = "aarch64", target_os = "macos"),
        all(target_arch = "x86_64", target_os = "windows"),
    )))]
    {
        "unknown-target"
    }
}

/// Cache key for a loaded native plugin artifact. Mirrors
/// [`otap_df_plugin_host::ComponentCacheKey`] but for the native ABI.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct NativeCacheKey {
    /// Hex-encoded SHA-256 of the cdylib artifact.
    pub artifact_sha256: String,
    /// Native plugin ABI version the host expects.
    pub abi_version: u32,
    /// Target triple the cdylib must have been built for.
    pub target_triple: String,
    /// Plugin API (descriptor schema) version.
    pub plugin_api_version: PluginApiVersion,
}

/// Configuration for the native plugin host.
#[derive(Clone, Debug, Default)]
pub struct NativePluginHostConfig {
    /// Directories scanned for native plugin manifests (`*.yaml` /
    /// `*.yml`). The host filters down to manifests with
    /// `kind: NativePlugin` so directories may be shared with the wasm
    /// host.
    pub plugin_dirs: Vec<PathBuf>,
    /// When `true`, unsigned plugins are rejected at load time.
    pub require_signed: bool,
}

/// One successfully loaded native plugin.
pub struct LoadedNativePlugin {
    /// The manifest as parsed and resolved against disk.
    pub manifest: LoadedManifest,
    /// The plugin descriptor returned by `descriptor()`.
    pub descriptor: PluginDescriptor,
    /// Per-component fingerprints derived from manifest + descriptor.
    pub fingerprints: Vec<PluginFingerprint>,
    /// Per-component config validators.
    pub validators: Vec<(String, Arc<NativePluginConfigValidator>)>,
    /// Cache key for the loaded artifact.
    pub cache_key: NativeCacheKey,
    /// Shared library handle. Used by the native-nodes adapter to
    /// construct per-instance state on demand.
    pub library: Arc<SharedPluginLibrary>,
}

impl std::fmt::Debug for LoadedNativePlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadedNativePlugin")
            .field("manifest", &self.manifest.manifest_path)
            .field("descriptor", &self.descriptor)
            .field("fingerprints", &self.fingerprints)
            .field("cache_key", &self.cache_key)
            .finish_non_exhaustive()
    }
}

impl LoadedNativePlugin {
    /// Construct a fresh per-node plugin instance + runner. Each
    /// pipeline node owns one of these for its lifetime.
    pub fn new_processor_runner(
        &self,
        urn: &str,
        config_json: &str,
    ) -> Result<NativeProcessorRunnerImpl, String> {
        let inst = PluginInstanceHandle::new(Arc::clone(&self.library), urn, config_json)?;
        Ok(NativeProcessorRunnerImpl { instance: inst })
    }

    /// Find the validator entry for a component URN.
    #[must_use]
    pub fn validator(&self, urn: &str) -> Option<Arc<NativePluginConfigValidator>> {
        self.validators
            .iter()
            .find(|(u, _)| u == urn)
            .map(|(_, v)| Arc::clone(v))
    }

    /// Find the descriptor entry for a component URN.
    #[must_use]
    pub fn component(&self, urn: &str) -> Option<&ComponentDescriptor> {
        self.descriptor.components.iter().find(|c| c.urn == urn)
    }
}

/// Native plugin host.
pub struct NativePluginHost {
    config: NativePluginHostConfig,
}

impl NativePluginHost {
    /// Construct a new host with the given config.
    #[must_use]
    pub fn new(config: NativePluginHostConfig) -> Self {
        Self { config }
    }

    /// Convenience: a host with no plugin directories.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(NativePluginHostConfig::default())
    }

    /// Borrow the host configuration.
    #[must_use]
    pub fn config(&self) -> &NativePluginHostConfig {
        &self.config
    }

    /// Discover and load every native-kind plugin in the configured
    /// directories.
    pub fn load_all(&self) -> Result<Vec<LoadedNativePlugin>, PluginError> {
        if self.config.plugin_dirs.is_empty() {
            return Ok(Vec::new());
        }
        let mut loaded = Vec::new();
        for dir in &self.config.plugin_dirs {
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let is_yaml = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e == "yaml" || e == "yml")
                    .unwrap_or(false);
                if !is_yaml {
                    continue;
                }
                let manifest = load_manifest(&path)?;
                if matches!(&manifest.manifest.runtime, RuntimeSpec::NativeCdylib { .. }) {
                    let l = self.load_one_resolved(manifest)?;
                    loaded.push(l);
                }
                // Wasm manifests are silently skipped — the wasm host
                // will load them through its own discovery.
            }
        }
        Ok(loaded)
    }

    /// Load a single manifest path. Returns an error if the manifest
    /// does not declare a native cdylib runtime.
    pub fn load_one(
        &self,
        manifest_path: &std::path::Path,
    ) -> Result<LoadedNativePlugin, PluginError> {
        let manifest = load_manifest(manifest_path)?;
        if !matches!(&manifest.manifest.runtime, RuntimeSpec::NativeCdylib { .. }) {
            return Err(PluginError::ManifestParse(format!(
                "native plugin host received non-native manifest: {}",
                manifest_path.display()
            )));
        }
        self.load_one_resolved(manifest)
    }

    fn load_one_resolved(
        &self,
        manifest: LoadedManifest,
    ) -> Result<LoadedNativePlugin, PluginError> {
        verify_artifact_sha256(&manifest)?;
        verify_artifact_signature(&manifest, self.config.require_signed)?;

        let library = open_library(&manifest.artifact_path)?;
        let vtable_ptr = call_register(&library)?;
        // SAFETY: vtable_ptr is non-null and points into the loaded
        // library, which is kept alive for the rest of this function.
        let vtable_ref: &OtapPluginVTable = unsafe { &*vtable_ptr };
        validate_vtable(vtable_ref).map_err(PluginError::ManifestParse)?;

        let descriptor = call_descriptor(vtable_ref)?;
        verify_native_phase1_descriptor(&descriptor)?;

        // Phase-1 plugin api version sanity: must be compatible with
        // the host's PluginApiVersion. The descriptor carries the
        // plugin's declared api version; the host's is constant.
        if !PLUGIN_API_VERSION.is_compatible_with(&descriptor.plugin_api_version) {
            return Err(PluginError::IncompatibleApiVersion {
                host: PLUGIN_API_VERSION.to_string(),
                plugin: descriptor.plugin_api_version.to_string(),
            });
        }

        let sha = match &manifest.manifest.runtime {
            RuntimeSpec::NativeCdylib { sha256, .. } => sha256.to_lowercase(),
            // load_one_resolved is only called for NativeCdylib runtimes.
            RuntimeSpec::WasmtimeComponent { .. } => {
                return Err(PluginError::ManifestParse(
                    "internal error: wasm runtime reached native loader".into(),
                ));
            }
        };

        let cache_key = NativeCacheKey {
            artifact_sha256: sha.clone(),
            abi_version: OTAP_PLUGIN_ABI_VERSION_V1,
            target_triple: target_triple().to_string(),
            plugin_api_version: descriptor.plugin_api_version,
        };

        let library_arc = Arc::new(SharedPluginLibrary {
            library,
            vtable: vtable_ptr,
        });

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

        let validators: Vec<(String, Arc<NativePluginConfigValidator>)> = descriptor
            .components
            .iter()
            .map(|c| {
                (
                    c.urn.clone(),
                    Arc::new(NativePluginConfigValidator {
                        library: Arc::clone(&library_arc),
                        urn: c.urn.clone(),
                    }),
                )
            })
            .collect();

        Ok(LoadedNativePlugin {
            manifest,
            descriptor,
            fingerprints,
            validators,
            cache_key,
            library: library_arc,
        })
    }
}

#[cfg(unix)]
fn open_library(artifact: &std::path::Path) -> Result<Library, PluginError> {
    // Use RTLD_LOCAL | RTLD_NOW to match the documented C-ABI conventions:
    //  - RTLD_LOCAL: symbols are not promoted to the process global
    //    namespace, preventing two plugins from leaking C-runtime state
    //    into each other.
    //  - RTLD_NOW: bind every undefined symbol up-front so missing
    //    symbols surface at load instead of at first call.
    // SAFETY: dlopen of a file under operator control. The on-disk
    // sha256 was already verified.
    let unix = unsafe {
        UnixLibrary::open(Some(artifact), RTLD_LOCAL | RTLD_NOW)
            .map_err(|e| PluginError::ManifestParse(format!("dlopen failed: {e}")))?
    };
    Ok(Library::from(unix))
}

#[cfg(not(unix))]
fn open_library(artifact: &std::path::Path) -> Result<Library, PluginError> {
    // SAFETY: same trust posture as the unix path; no RTLD_* flags on
    // non-unix platforms.
    let lib = unsafe { Library::new(artifact) }
        .map_err(|e| PluginError::ManifestParse(format!("LoadLibrary failed: {e}")))?;
    Ok(lib)
}

fn call_register(library: &Library) -> Result<*const OtapPluginVTable, PluginError> {
    // SAFETY: symbol type is the documented `OtapPluginRegisterV1`
    // ABI. Symbol must be looked up by NUL-terminated bytes per
    // libloading's contract.
    let symbol: libloading::Symbol<'_, OtapPluginRegisterV1> = unsafe {
        library.get(REGISTER_SYMBOL_V1).map_err(|e| {
            PluginError::ManifestParse(format!(
                "missing entry symbol `otap_plugin_register_v1`: {e}"
            ))
        })?
    };
    // SAFETY: the plugin's register fn is documented as a no-args
    // pointer-returning function with no side effects.
    let ptr = unsafe { symbol() };
    if ptr.is_null() {
        return Err(PluginError::ManifestParse(
            "plugin register fn returned NULL vtable".into(),
        ));
    }
    Ok(ptr)
}

fn call_descriptor(vtable: &OtapPluginVTable) -> Result<PluginDescriptor, PluginError> {
    let mut raw = OtapPluginDescriptorRaw {
        abi_version: 0,
        name_ptr: std::ptr::null(),
        name_len: 0,
        version_ptr: std::ptr::null(),
        version_len: 0,
        plugin_api_major: 0,
        plugin_api_minor: 0,
        components_json_ptr: std::ptr::null(),
        components_json_len: 0,
    };
    // SAFETY: vtable was checked by `validate_vtable` before we got
    // here, so the descriptor slot is `Some`. Plugin panics are not
    // caught — see the Panic Contract in `otap-df-plugin-abi`.
    let descriptor_fn = vtable
        .descriptor
        .expect("descriptor slot present (validated at load)");
    let rc = unsafe { descriptor_fn(&mut raw) };
    if rc != otap_df_plugin_abi::HOST_OK {
        return Err(PluginError::ManifestParse(format!(
            "plugin descriptor() returned non-zero rc={rc}"
        )));
    }
    if raw.abi_version != OTAP_PLUGIN_ABI_VERSION_V1 {
        return Err(PluginError::ManifestParse(format!(
            "plugin descriptor reports ABI {}, host expects {}",
            raw.abi_version, OTAP_PLUGIN_ABI_VERSION_V1
        )));
    }
    // SAFETY: the plugin promised valid pointers + lengths for these
    // fields. We copy out into owned Strings before returning so the
    // borrowed memory can be invalidated immediately.
    let name = copy_str(raw.name_ptr, raw.name_len)?;
    let version = copy_str(raw.version_ptr, raw.version_len)?;
    let components_json = copy_str(raw.components_json_ptr, raw.components_json_len)?;
    let components: Vec<ComponentDescriptor> = serde_json::from_str(&components_json)
        .map_err(|e| PluginError::ManifestParse(format!("descriptor components JSON: {e}")))?;

    Ok(PluginDescriptor {
        name,
        version,
        plugin_api_version: PluginApiVersion::new(raw.plugin_api_major, raw.plugin_api_minor),
        components,
    })
}

fn copy_str(ptr: *const u8, len: usize) -> Result<String, PluginError> {
    if ptr.is_null() {
        return Err(PluginError::ManifestParse(
            "plugin descriptor returned null string pointer".into(),
        ));
    }
    // SAFETY: caller-documented non-null pointer + length.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|e| PluginError::ManifestParse(format!("descriptor utf-8: {e}")))
}

/// Phase-1 native invariants:
///   * processors only;
///   * single-output;
///   * declared `OtlpProtoBytes` payload support (since accessors are
///     OTLP-aware in phase 1).
pub(crate) fn verify_native_phase1_descriptor(
    descriptor: &PluginDescriptor,
) -> Result<(), PluginError> {
    use otap_df_plugin_api::{ComponentKind, OutputArity, PayloadFormat};
    if descriptor.components.is_empty() {
        return Err(PluginError::ManifestParse(
            "native plugin descriptor has no components".into(),
        ));
    }
    for c in &descriptor.components {
        match c.kind {
            ComponentKind::Processor => {}
            other => {
                return Err(PluginError::ManifestParse(format!(
                    "native plugin component {} has unsupported kind {:?} in phase 1 (processors only)",
                    c.urn, other
                )));
            }
        }
        if !matches!(c.output_arity, OutputArity::Single) {
            return Err(PluginError::ManifestParse(format!(
                "native plugin processor {} declares multi-output; phase 1 supports single-output only",
                c.urn
            )));
        }
        if !c
            .supported_payloads
            .iter()
            .any(|f| matches!(f, PayloadFormat::OtlpProtoBytes))
        {
            return Err(PluginError::UnsupportedPayloadFormat {
                format: format!("{} declares no otlp-proto-bytes support", c.urn),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use otap_df_plugin_api::{
        ComponentDescriptor, ComponentKind, OutputArity, PayloadFormat, PluginApiVersion,
    };

    fn d(kind: ComponentKind, arity: OutputArity, fmts: Vec<PayloadFormat>) -> PluginDescriptor {
        PluginDescriptor {
            name: "p".into(),
            version: "0.1.0".into(),
            plugin_api_version: PluginApiVersion::new(0, 1),
            components: vec![ComponentDescriptor {
                urn: "urn:test:processor:p".into(),
                kind,
                supported_payloads: fmts,
                output_arity: arity,
                config_schema_json: None,
            }],
        }
    }

    #[test]
    fn rejects_exporter_in_phase1() {
        let desc = d(
            ComponentKind::Exporter,
            OutputArity::Single,
            vec![PayloadFormat::OtlpProtoBytes],
        );
        assert!(verify_native_phase1_descriptor(&desc).is_err());
    }

    #[test]
    fn rejects_multi_output() {
        let desc = d(
            ComponentKind::Processor,
            OutputArity::Multi,
            vec![PayloadFormat::OtlpProtoBytes],
        );
        let err = verify_native_phase1_descriptor(&desc).unwrap_err();
        assert!(format!("{err:?}").contains("multi-output"));
    }

    #[test]
    fn rejects_arrow_only() {
        let desc = d(
            ComponentKind::Processor,
            OutputArity::Single,
            vec![PayloadFormat::OtapArrowIpc],
        );
        assert!(verify_native_phase1_descriptor(&desc).is_err());
    }

    #[test]
    fn accepts_single_output_processor() {
        let desc = d(
            ComponentKind::Processor,
            OutputArity::Single,
            vec![PayloadFormat::OtlpProtoBytes],
        );
        verify_native_phase1_descriptor(&desc).unwrap();
    }
}
