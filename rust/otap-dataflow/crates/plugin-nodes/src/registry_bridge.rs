// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Translate `LoadedPlugin` values from the host into a
//! `DynamicComponentRegistry<OtapPdata>` consumable by the engine overlay.

use std::sync::Arc;

use otap_df_engine::runtime_registry::{
    ConfigValidator, DuplicateUrn, DynamicComponentRegistry, DynamicExporterEntry,
    DynamicNodeFingerprint, DynamicProcessorEntry,
};
use otap_df_engine::wiring_contract::WiringContract;
use otap_df_otap::pdata::OtapPdata;
use otap_df_plugin_api::{ComponentKind, PluginFingerprint};
use otap_df_plugin_host::{LoadedPlugin, PluginConfigValidator};
use tokio::sync::Semaphore;

/// Build a dynamic registry from a set of loaded plugins.
///
/// `exporter_blocking_permits` is the host-wide concurrency cap for
/// plugin exporter blocking-pool dispatch. It is cloned once into each
/// dynamic exporter factory (and from there into each
/// `WasmExporterAdapter`), so all plugin-backed exporters share the same
/// limit. Build the value via [`PluginHost::exporter_blocking_permits`]:
///
/// ```ignore
/// let host = PluginHost::new(cfg);
/// let plugins = host.load_all()?;
/// let registry = build_dynamic_registry(&plugins, host.exporter_blocking_permits())?;
/// ```
///
/// Each component contributes:
///   * its URN as an owned key
///   * a [`ConfigValidator::Dynamic`] backed by the plugin's
///     `validate-config` export (real Wasmtime call when the
///     `wasmtime-backend` feature is enabled in `plugin-host`,
///     fail-closed stub otherwise)
///   * a [`DynamicNodeFingerprint`] for live-reconfig identity comparison
///   * a real factory built by `make_processor_factory` /
///     `make_exporter_factory` (when the `wasmtime-backend` feature is
///     enabled and the plugin exposes a `PluginRunner`); without that,
///     `factory: None` and the pipeline build rejects the URN.
pub fn build_dynamic_registry(
    plugins: &[LoadedPlugin],
    exporter_blocking_permits: Arc<Semaphore>,
) -> Result<DynamicComponentRegistry<OtapPdata>, DuplicateUrn> {
    let mut reg = DynamicComponentRegistry::<OtapPdata>::empty();

    for plugin in plugins {
        for component in &plugin.descriptor.components {
            let fingerprint = match plugin
                .fingerprints
                .iter()
                .find(|f| f.component_urn == component.urn)
            {
                Some(f) => to_engine_fingerprint(f),
                None => DynamicNodeFingerprint {
                    component_urn: component.urn.clone(),
                    plugin_version: plugin.manifest.manifest.metadata.version.clone(),
                    artifact_sha256: plugin.cache_key.artifact_sha256.clone(),
                    plugin_api_version: plugin.descriptor.plugin_api_version.to_string(),
                },
            };

            let urn: Arc<str> = Arc::from(component.urn.as_str());
            let validator_obj = plugin
                .validators
                .iter()
                .find(|(u, _)| u == &component.urn)
                .map(|(_, v)| Arc::clone(v));
            let validator = make_validator(validator_obj);

            match component.kind {
                ComponentKind::Processor => {
                    // Phase-1 RFC: plugin processors are single/default-output
                    // only. Enforced by `verify_phase1_descriptor` in
                    // plugin-host (rejects `OutputArity::Multi`), so any
                    // processor reaching this point is already single-output.
                    let factory = crate::factory::make_processor_factory(plugin, component);
                    reg.register_processor(DynamicProcessorEntry {
                        urn: urn.clone(),
                        validator,
                        fingerprint,
                        wiring_contract: WiringContract::UNRESTRICTED,
                        factory,
                    })?;
                }
                ComponentKind::Exporter => {
                    let factory = crate::factory::make_exporter_factory(
                        plugin,
                        component,
                        Arc::clone(&exporter_blocking_permits),
                    );
                    reg.register_exporter(DynamicExporterEntry {
                        urn: urn.clone(),
                        validator,
                        fingerprint,
                        wiring_contract: WiringContract::UNRESTRICTED,
                        factory,
                    })?;
                }
                ComponentKind::Receiver | ComponentKind::Extension => {
                    // Already filtered by plugin-host descriptor verifier;
                    // skip defensively.
                    continue;
                }
            }
        }
    }

    Ok(reg)
}

/// Wrap a plugin-side validator into the engine's `ConfigValidator::Dynamic`.
///
/// When no validator was registered for the component (which would only
/// happen if `LoadedPlugin::validators` is somehow out of sync with
/// `descriptor.components`), fall back to a fail-closed validator so we
/// never silently accept a config we can't actually validate.
fn make_validator(plugin_validator: Option<Arc<dyn PluginConfigValidator>>) -> ConfigValidator {
    let validator = plugin_validator.unwrap_or_else(|| {
        Arc::new(otap_df_plugin_host::UnimplementedValidator) as Arc<dyn PluginConfigValidator>
    });
    ConfigValidator::Dynamic(Arc::new(move |config| {
        let json = serde_json::to_string(config).map_err(|e| {
            otap_df_config::error::Error::InvalidUserConfig {
                error: format!("failed to re-serialize plugin config to JSON: {e}"),
            }
        })?;
        validator
            .validate(&json)
            .map_err(|message| otap_df_config::error::Error::InvalidUserConfig { error: message })
    }))
}

fn to_engine_fingerprint(api: &PluginFingerprint) -> DynamicNodeFingerprint {
    DynamicNodeFingerprint {
        component_urn: api.component_urn.clone(),
        plugin_version: api.plugin_version.clone(),
        artifact_sha256: api.artifact_sha256.clone(),
        plugin_api_version: api.plugin_api_version.to_string(),
    }
}
