// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Translate `LoadedNativePlugin` values from the native host into a
//! `DynamicComponentRegistry<OtapPdata>` consumable by the engine
//! overlay.
//!
//! Phase 1 only contributes processor entries.

use std::sync::Arc;

use otap_df_engine::processor::ProcessorWrapper;
use otap_df_engine::runtime_registry::{
    ConfigValidator, DuplicateUrn, DynamicComponentRegistry, DynamicNodeFingerprint,
    DynamicProcessorEntry, DynamicProcessorFactory,
};
use otap_df_engine::wiring_contract::WiringContract;
use otap_df_otap::pdata::OtapPdata;
use otap_df_plugin_api::{ComponentKind, PluginFingerprint};
use otap_df_plugin_native_host::{LoadedNativePlugin, NativePluginConfigValidator};

use crate::adapter::NativeProcessorAdapter;

/// Build a dynamic registry from a slice of loaded native plugins.
///
/// Each phase-1 component contributes a [`DynamicProcessorEntry`] with:
///   * the URN as an owned key
///   * a [`ConfigValidator::Dynamic`] backed by the plugin's
///     `validate-config` export
///   * a [`DynamicNodeFingerprint`] for live-reconfig identity
///   * a real factory built via `make_processor_factory`
pub fn build_native_registry(
    plugins: &[LoadedNativePlugin],
) -> Result<DynamicComponentRegistry<OtapPdata>, DuplicateUrn> {
    let mut reg = DynamicComponentRegistry::<OtapPdata>::empty();
    extend_native_registry(&mut reg, plugins)?;
    Ok(reg)
}

/// Extend an existing dynamic registry with the contributions of every
/// loaded native plugin.
///
/// Used by the binary wiring to merge native plugin entries into the
/// dynamic registry produced by the wasm path, so both backends share a
/// single overlay layered on top of the static `OTAP_PIPELINE_FACTORY`.
///
/// Returns `Err(DuplicateUrn)` if any URN is already present in the
/// registry — including a URN already registered by the wasm path. This
/// is the intended behavior: two backends advertising the same component
/// URN is an operator misconfiguration that the static-first overlay
/// cannot disambiguate.
pub fn extend_native_registry(
    reg: &mut DynamicComponentRegistry<OtapPdata>,
    plugins: &[LoadedNativePlugin],
) -> Result<(), DuplicateUrn> {
    for plugin in plugins {
        for component in &plugin.descriptor.components {
            // Phase-1 native loader rejects non-processor components at
            // descriptor verification, so this branch is the only one
            // we can reach. We still match defensively.
            match component.kind {
                ComponentKind::Processor => {}
                _ => continue,
            }

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
            let validator = make_validator(plugin.validator(&component.urn));
            let factory = Some(make_processor_factory(plugin, &component.urn));

            reg.register_processor(DynamicProcessorEntry {
                urn: urn.clone(),
                validator,
                fingerprint,
                wiring_contract: WiringContract::UNRESTRICTED,
                factory,
            })?;
        }
    }
    Ok(())
}

/// Build a single dynamic processor factory closure for a plugin
/// component. The closure captures a clone of the plugin's library
/// `Arc` plus the URN so it can spin up a fresh per-node instance on
/// every pipeline build.
fn make_processor_factory(
    plugin: &LoadedNativePlugin,
    urn: &str,
) -> DynamicProcessorFactory<OtapPdata> {
    let library = Arc::clone(&plugin.library);
    let urn_arc: Arc<str> = Arc::from(urn);
    let fingerprint = plugin
        .fingerprints
        .iter()
        .find(|f| f.component_urn == urn)
        .cloned()
        .expect("fingerprint missing for native component");
    let cache_key = plugin.cache_key.clone();

    Arc::new(move |_pipeline, node_id, node_config, processor_config| {
        let config_json = serde_json::to_string(&node_config.config).map_err(|e| {
            otap_df_config::error::Error::InvalidUserConfig {
                error: format!("native plugin processor: failed to serialize config: {e}"),
            }
        })?;
        // Construct a fresh per-node plugin instance.
        let inst = otap_df_plugin_native_host::runner::PluginInstanceHandle::new(
            Arc::clone(&library),
            urn_arc.as_ref(),
            &config_json,
        )
        .map_err(|msg| otap_df_config::error::Error::InvalidUserConfig { error: msg })?;
        let runner =
            Arc::new(otap_df_plugin_native_host::runner::NativeProcessorRunnerImpl::new(inst));
        let adapter = NativeProcessorAdapter {
            component_urn: urn_arc.clone(),
            fingerprint: fingerprint.clone(),
            cache_key: cache_key.clone(),
            config_json,
            runner,
            node_id: node_id.clone(),
        };
        Ok(ProcessorWrapper::local(
            adapter,
            node_id,
            node_config,
            processor_config,
        ))
    })
}

fn make_validator(plugin_validator: Option<Arc<NativePluginConfigValidator>>) -> ConfigValidator {
    let validator = plugin_validator;
    ConfigValidator::Dynamic(Arc::new(move |config| {
        let json = serde_json::to_string(config).map_err(|e| {
            otap_df_config::error::Error::InvalidUserConfig {
                error: format!("failed to re-serialize plugin config to JSON: {e}"),
            }
        })?;
        match validator.as_ref() {
            Some(v) => v.validate(&json).map_err(|message| {
                otap_df_config::error::Error::InvalidUserConfig { error: message }
            }),
            None => Err(otap_df_config::error::Error::InvalidUserConfig {
                error: "no validator wired for this native plugin URN".to_string(),
            }),
        }
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
