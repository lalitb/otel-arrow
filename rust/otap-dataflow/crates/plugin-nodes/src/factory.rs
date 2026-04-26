// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Build dynamic processor/exporter factories from a [`LoadedPlugin`].
//!
//! Phase-1: each factory closure captures a shared [`PluginRunner`] and the
//! component identity (URN, fingerprint, cache key, limits). On invocation
//! it pre-serializes the node's user config to JSON (the plugin already
//! accepted this exact JSON via `validate-config`) and constructs a
//! [`WasmProcessorAdapter`] / [`WasmExporterAdapter`] wrapped in the
//! engine's local wrapper.

use std::sync::Arc;

use otap_df_engine::exporter::ExporterWrapper;
use otap_df_engine::processor::ProcessorWrapper;
use otap_df_engine::runtime_registry::{DynamicExporterFactory, DynamicProcessorFactory};
use otap_df_otap::pdata::OtapPdata;
use otap_df_plugin_api::{ComponentDescriptor, PluginFingerprint};
use otap_df_plugin_host::LoadedPlugin;
use tokio::sync::Semaphore;

use crate::adapter::{WasmExporterAdapter, WasmProcessorAdapter};

fn pick_runner(
    plugin: &LoadedPlugin,
    urn: &str,
) -> Option<Arc<dyn otap_df_plugin_host::PluginRunner>> {
    plugin
        .runners
        .iter()
        .find(|(u, _)| u == urn)
        .map(|(_, r)| Arc::clone(r))
}

fn pick_fingerprint(plugin: &LoadedPlugin, urn: &str) -> PluginFingerprint {
    plugin
        .fingerprints
        .iter()
        .find(|f| f.component_urn == urn)
        .cloned()
        .unwrap_or_else(|| PluginFingerprint {
            component_urn: urn.to_string(),
            plugin_version: plugin.manifest.manifest.metadata.version.clone(),
            artifact_sha256: plugin.cache_key.artifact_sha256.clone(),
            plugin_api_version: plugin.descriptor.plugin_api_version,
        })
}

/// Build a dynamic processor factory for the given plugin component.
///
/// Returns `None` if the plugin host did not produce a runner for this
/// component (which would only happen if `runners` is out of sync with
/// `descriptor.components` — the registry bridge will then fall through to
/// the build-time rejection path).
#[must_use]
pub fn make_processor_factory(
    plugin: &LoadedPlugin,
    component: &ComponentDescriptor,
) -> Option<DynamicProcessorFactory<OtapPdata>> {
    let runner = pick_runner(plugin, &component.urn)?;
    let urn: Arc<str> = Arc::from(component.urn.as_str());
    let fingerprint = pick_fingerprint(plugin, &component.urn);
    let cache_key = plugin.cache_key.clone();
    let limits = plugin.manifest.manifest.limits.clone();

    Some(Arc::new(
        move |_pipeline, node_id, node_config, processor_config| {
            let config_json = serde_json::to_string(&node_config.config).map_err(|e| {
                otap_df_config::error::Error::InvalidUserConfig {
                    error: format!("plugin processor: failed to serialize config: {e}"),
                }
            })?;
            let adapter = WasmProcessorAdapter {
                component_urn: urn.clone(),
                fingerprint: fingerprint.clone(),
                cache_key: cache_key.clone(),
                limits: limits.clone(),
                config_json,
                runner: Arc::clone(&runner),
                node_id: node_id.clone(),
            };
            Ok(ProcessorWrapper::local(
                adapter,
                node_id,
                node_config,
                processor_config,
            ))
        },
    ))
}

/// Build a dynamic exporter factory for the given plugin component.
#[must_use]
pub fn make_exporter_factory(
    plugin: &LoadedPlugin,
    component: &ComponentDescriptor,
    blocking_permits: Arc<Semaphore>,
) -> Option<DynamicExporterFactory<OtapPdata>> {
    let runner = pick_runner(plugin, &component.urn)?;
    let urn: Arc<str> = Arc::from(component.urn.as_str());
    let fingerprint = pick_fingerprint(plugin, &component.urn);
    let cache_key = plugin.cache_key.clone();
    let limits = plugin.manifest.manifest.limits.clone();

    Some(Arc::new(
        move |_pipeline, node_id, node_config, exporter_config| {
            let config_json = serde_json::to_string(&node_config.config).map_err(|e| {
                otap_df_config::error::Error::InvalidUserConfig {
                    error: format!("plugin exporter: failed to serialize config: {e}"),
                }
            })?;
            let adapter = WasmExporterAdapter {
                component_urn: urn.clone(),
                fingerprint: fingerprint.clone(),
                cache_key: cache_key.clone(),
                limits: limits.clone(),
                config_json,
                runner: Arc::clone(&runner),
                node_id: node_id.clone(),
                blocking_permits: Arc::clone(&blocking_permits),
            };
            Ok(ExporterWrapper::local(
                adapter,
                node_id,
                node_config,
                exporter_config,
            ))
        },
    ))
}
