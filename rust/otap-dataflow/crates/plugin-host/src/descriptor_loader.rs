// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Plugin descriptor loader.
//!
//! When the `wasmtime-backend` feature is enabled, this module compiles the
//! component, instantiates it, calls the `descriptor()` export, and decodes
//! the JSON-encoded result.
//!
//! Without that feature it returns [`PluginError::BackendUnimplemented`].

use std::path::Path;

use otap_df_plugin_api::{PluginDescriptor, PluginError};

/// Load the plugin descriptor from a precompiled component on disk.
///
/// With `wasmtime-backend` enabled this is a real call into the plugin;
/// without it, returns `PluginError::BackendUnimplemented`.
pub fn load_descriptor(_artifact: &Path) -> Result<PluginDescriptor, PluginError> {
    #[cfg(feature = "wasmtime-backend")]
    {
        let loaded = crate::wasmtime_backend::load_component(_artifact)?;
        crate::wasmtime_backend::call_descriptor(&loaded)
    }

    #[cfg(not(feature = "wasmtime-backend"))]
    {
        Err(PluginError::BackendUnimplemented(
            "descriptor() invocation requires the `wasmtime-backend` feature",
        ))
    }
}

/// Reject descriptors that violate phase-1 constraints.
///
/// Rules (RFC §4 / §7.4):
///   * at least one component is processor or exporter
///   * each phase-1 component must support `OtlpProtoBytes`
///   * processor components must declare single/default output arity
///   * receivers / extensions are rejected even if declared
pub fn verify_phase1_descriptor(descriptor: &PluginDescriptor) -> Result<(), PluginError> {
    use otap_df_plugin_api::{ComponentKind, OutputArity, PayloadFormat};

    if descriptor.components.is_empty() {
        return Err(PluginError::ManifestParse(
            "plugin descriptor has no components".into(),
        ));
    }

    for c in &descriptor.components {
        match c.kind {
            ComponentKind::Processor => {
                if !matches!(c.output_arity, OutputArity::Single) {
                    return Err(PluginError::ManifestParse(format!(
                        "processor {} declares multi-output; phase 1 supports \
                         single/default-output only",
                        c.urn
                    )));
                }
            }
            ComponentKind::Exporter => {}
            ComponentKind::Receiver | ComponentKind::Extension => {
                return Err(PluginError::ManifestParse(format!(
                    "component {} has unsupported kind {:?} in phase 1",
                    c.urn, c.kind
                )));
            }
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
        PluginDescriptor,
    };

    fn d(kinds: Vec<(ComponentKind, Vec<PayloadFormat>, OutputArity)>) -> PluginDescriptor {
        PluginDescriptor {
            name: "p".into(),
            version: "0.1.0".into(),
            plugin_api_version: PluginApiVersion::new(0, 1),
            components: kinds
                .into_iter()
                .enumerate()
                .map(|(i, (kind, fmts, arity))| ComponentDescriptor {
                    urn: format!("urn:test:processor:c{i}"),
                    kind,
                    supported_payloads: fmts,
                    output_arity: arity,
                    config_schema_json: None,
                })
                .collect(),
        }
    }

    #[test]
    fn rejects_receiver() {
        let desc = d(vec![(
            ComponentKind::Receiver,
            vec![PayloadFormat::OtlpProtoBytes],
            OutputArity::Single,
        )]);
        assert!(verify_phase1_descriptor(&desc).is_err());
    }

    #[test]
    fn rejects_arrow_only() {
        let desc = d(vec![(
            ComponentKind::Processor,
            vec![PayloadFormat::OtapArrowIpc],
            OutputArity::Single,
        )]);
        assert!(verify_phase1_descriptor(&desc).is_err());
    }

    #[test]
    fn rejects_multi_output_processor() {
        let desc = d(vec![(
            ComponentKind::Processor,
            vec![PayloadFormat::OtlpProtoBytes],
            OutputArity::Multi,
        )]);
        let err = verify_phase1_descriptor(&desc).unwrap_err();
        assert!(format!("{err:?}").contains("multi-output"));
    }

    #[test]
    fn accepts_processor_with_otlp() {
        let desc = d(vec![(
            ComponentKind::Processor,
            vec![PayloadFormat::OtlpProtoBytes, PayloadFormat::OtapArrowIpc],
            OutputArity::Single,
        )]);
        verify_phase1_descriptor(&desc).unwrap();
    }
}
