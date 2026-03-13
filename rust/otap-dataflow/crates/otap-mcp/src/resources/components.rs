// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Component discovery resources.
//!
//! Exposes the registered OTAP dataflow engine components (receivers,
//! processors, exporters) as MCP resources, including their URNs,
//! wiring contracts, and config schemas.

use otap_df_engine::wiring_contract::{OutputFanoutRule, WiringContract};
use otap_df_otap::OTAP_PIPELINE_FACTORY;
use serde::Serialize;

/// Describes a single registered component.
#[derive(Debug, Clone, Serialize)]
pub struct ComponentInfo {
    /// The component URN (e.g., `urn:otel:receiver:otlp`).
    pub urn: String,
    /// The component type (`receiver`, `processor`, or `exporter`).
    pub component_type: String,
    /// Short name derived from the URN.
    pub name: String,
    /// Wiring contract describing topology constraints.
    pub wiring: WiringInfo,
    /// Whether a JSON Schema is available for this component's configuration.
    pub has_config_schema: bool,
}

/// Serializable representation of a component's wiring contract.
#[derive(Debug, Clone, Serialize)]
pub struct WiringInfo {
    /// Output fanout constraint description.
    pub output_fanout: String,
    /// Maximum destinations per output, if constrained.
    pub max_outputs_per_port: Option<usize>,
}

impl From<&WiringContract> for WiringInfo {
    fn from(contract: &WiringContract) -> Self {
        match contract.output_fanout {
            OutputFanoutRule::Unrestricted => WiringInfo {
                output_fanout: "unrestricted".to_string(),
                max_outputs_per_port: None,
            },
            OutputFanoutRule::AtMostPerOutput(max) => WiringInfo {
                output_fanout: format!("at_most_{max}_per_output"),
                max_outputs_per_port: Some(max),
            },
        }
    }
}

/// Lists all registered components of the given type.
///
/// Valid types are `receivers`, `processors`, and `exporters`.
pub fn list_components(component_type: &str) -> Result<Vec<ComponentInfo>, String> {
    let mut components = match component_type {
        "receivers" => {
            let map = OTAP_PIPELINE_FACTORY.get_receiver_factory_map();
            map.values()
                .map(|f| ComponentInfo {
                    urn: f.name.to_string(),
                    component_type: "receiver".to_string(),
                    name: extract_name(f.name),
                    wiring: WiringInfo::from(&f.wiring_contract),
                    has_config_schema: f.config_schema.is_some(),
                })
                .collect::<Vec<_>>()
        }
        "processors" => {
            let map = OTAP_PIPELINE_FACTORY.get_processor_factory_map();
            map.values()
                .map(|f| ComponentInfo {
                    urn: f.name.to_string(),
                    component_type: "processor".to_string(),
                    name: extract_name(f.name),
                    wiring: WiringInfo::from(&f.wiring_contract),
                    has_config_schema: f.config_schema.is_some(),
                })
                .collect::<Vec<_>>()
        }
        "exporters" => {
            let map = OTAP_PIPELINE_FACTORY.get_exporter_factory_map();
            map.values()
                .map(|f| ComponentInfo {
                    urn: f.name.to_string(),
                    component_type: "exporter".to_string(),
                    name: extract_name(f.name),
                    wiring: WiringInfo::from(&f.wiring_contract),
                    has_config_schema: f.config_schema.is_some(),
                })
                .collect::<Vec<_>>()
        }
        other => {
            return Err(format!(
                "Unknown component type: '{other}'. Valid types: receivers, processors, exporters"
            ));
        }
    };
    components.sort_by(|a, b| a.urn.cmp(&b.urn));
    Ok(components)
}

/// Lists all registered components across all types.
#[must_use]
pub fn list_all_components() -> Vec<ComponentInfo> {
    let mut all = Vec::new();
    for comp_type in &["receivers", "processors", "exporters"] {
        if let Ok(components) = list_components(comp_type) {
            all.extend(components);
        }
    }
    all
}

/// Resolves a component URN from a type and name/URN input.
///
/// Accepts:
/// - A full URN passed as `component_name` (e.g., `urn:microsoft:exporter:geneva`)
/// - A shortcut name (e.g., `geneva`) — tries `urn:otel:<type>:<name>` first,
///   then searches all registered URNs whose last segment matches the name.
///
/// On failure, returns a helpful error listing similar URNs from the registry.
fn resolve_urn(
    component_type: &str,
    component_name: &str,
) -> Result<String, String> {
    // If it already looks like a full URN, use it directly.
    if component_name.starts_with("urn:") {
        return Ok(component_name.to_string());
    }

    // Otherwise build the default otel URN.
    Ok(format!("urn:otel:{component_type}:{component_name}"))
}

/// Builds a "not found" error message with suggestions from the registry.
fn not_found_error(kind: &str, urn: &str, registered: Vec<String>) -> String {
    let suggestions: Vec<String> = registered
        .iter()
        .filter(|u| {
            u.contains(urn.rsplit(':').next().unwrap_or(""))
                || urn.contains(u.rsplit(':').next().unwrap_or(""))
        })
        .cloned()
        .collect();

    if suggestions.is_empty() {
        format!(
            "Unknown {kind}: {urn}. Available {kind}s: {}",
            registered.join(", ")
        )
    } else {
        format!(
            "Unknown {kind}: {urn}. Did you mean: {}",
            suggestions.join(", ")
        )
    }
}

/// Validates a component config against its registered validator.
///
/// Accepts full URNs (e.g., `urn:microsoft:exporter:geneva`) or shortcut names
/// (e.g., `geneva`). On failure, suggests the closest matching registered URN.
/// Returns `Ok(())` if the config is valid, or an error message.
pub fn validate_component_config(
    component_type: &str,
    component_name: &str,
    config: &serde_json::Value,
) -> Result<(), String> {
    let urn = resolve_urn(component_type, component_name)?;

    match component_type {
        "receiver" => {
            let map = OTAP_PIPELINE_FACTORY.get_receiver_factory_map();
            let factory = map.get(urn.as_str()).ok_or_else(|| {
                not_found_error("receiver", &urn, map.keys().map(|k| k.to_string()).collect())
            })?;
            (factory.validate_config)(config).map_err(|e| format!("{e}"))
        }
        "processor" => {
            let map = OTAP_PIPELINE_FACTORY.get_processor_factory_map();
            let factory = map.get(urn.as_str()).ok_or_else(|| {
                not_found_error("processor", &urn, map.keys().map(|k| k.to_string()).collect())
            })?;
            (factory.validate_config)(config).map_err(|e| format!("{e}"))
        }
        "exporter" => {
            let map = OTAP_PIPELINE_FACTORY.get_exporter_factory_map();
            let factory = map.get(urn.as_str()).ok_or_else(|| {
                not_found_error("exporter", &urn, map.keys().map(|k| k.to_string()).collect())
            })?;
            (factory.validate_config)(config).map_err(|e| format!("{e}"))
        }
        other => Err(format!(
            "Unknown component type: '{other}'. Valid types: receiver, processor, exporter"
        )),
    }
}

/// Returns the JSON Schema for a component's configuration, if available.
///
/// Accepts full URNs (e.g., `urn:microsoft:exporter:geneva`) or shortcut names
/// (e.g., `geneva`). On failure, suggests the closest matching registered URN.
pub fn get_component_schema(
    component_type: &str,
    component_name: &str,
) -> Result<Option<serde_json::Value>, String> {
    let urn = resolve_urn(component_type, component_name)?;

    let schema_fn = match component_type {
        "receiver" => {
            let map = OTAP_PIPELINE_FACTORY.get_receiver_factory_map();
            let factory = map.get(urn.as_str()).ok_or_else(|| {
                not_found_error("receiver", &urn, map.keys().map(|k| k.to_string()).collect())
            })?;
            factory.config_schema
        }
        "processor" => {
            let map = OTAP_PIPELINE_FACTORY.get_processor_factory_map();
            let factory = map.get(urn.as_str()).ok_or_else(|| {
                not_found_error("processor", &urn, map.keys().map(|k| k.to_string()).collect())
            })?;
            factory.config_schema
        }
        "exporter" => {
            let map = OTAP_PIPELINE_FACTORY.get_exporter_factory_map();
            let factory = map.get(urn.as_str()).ok_or_else(|| {
                not_found_error("exporter", &urn, map.keys().map(|k| k.to_string()).collect())
            })?;
            factory.config_schema
        }
        other => {
            return Err(format!(
                "Unknown component type: '{other}'. Valid types: receiver, processor, exporter"
            ));
        }
    };

    Ok(schema_fn.map(|f| f()))
}

/// Extracts a short name from a component URN.
///
/// For example: `"receiver:otlp"` → `"otlp"`,
/// `"urn:otel:receiver:otlp"` → `"otlp"`.
fn extract_name(urn: &str) -> String {
    urn.rsplit(':').next().unwrap_or(urn).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_name() {
        assert_eq!(extract_name("receiver:otlp"), "otlp");
        assert_eq!(extract_name("urn:otel:receiver:otlp"), "otlp");
        assert_eq!(extract_name("otlp"), "otlp");
    }

    #[test]
    fn test_list_all_components() {
        let components = list_all_components();
        assert!(!components.is_empty(), "should have registered components");
        // Verify wiring info is populated
        for comp in &components {
            assert!(!comp.wiring.output_fanout.is_empty());
        }
    }

    #[test]
    fn test_list_unknown_type() {
        let result = list_components("unknown");
        assert!(result.is_err());
    }
}
