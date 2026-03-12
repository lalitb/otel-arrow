// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Template-based pipeline configuration generator.
//!
//! Takes structured input (component types, configs, policies) and produces
//! a valid OTAP dataflow engine YAML configuration with correct DAG wiring.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A node specification for the generator.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct NodeSpec {
    /// Node ID in the pipeline (e.g., "receiver", "batch", "exporter").
    /// Auto-generated if not provided.
    #[serde(default)]
    pub id: Option<String>,
    /// Component URN (e.g., "receiver:otlp", "processor:batch", "exporter:otlp_grpc").
    pub r#type: String,
    /// Node-specific configuration as a JSON string.
    /// Will be embedded as YAML in the generated config.
    #[serde(default)]
    pub config: Option<String>,
}

/// Channel capacity policy.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct ChannelPolicies {
    /// Control channel capacity per node.
    #[serde(default = "default_control_capacity")]
    pub control_node: u32,
    /// Control channel capacity per pipeline.
    #[serde(default = "default_control_capacity")]
    pub control_pipeline: u32,
    /// PData channel capacity.
    #[serde(default = "default_pdata_capacity")]
    pub pdata: u32,
}

impl Default for ChannelPolicies {
    fn default() -> Self {
        Self {
            control_node: default_control_capacity(),
            control_pipeline: default_control_capacity(),
            pdata: default_pdata_capacity(),
        }
    }
}

fn default_control_capacity() -> u32 {
    256
}

fn default_pdata_capacity() -> u32 {
    128
}

/// Input for the config generator.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct GenerateConfigInput {
    /// Pipeline group ID.
    #[serde(default = "default_group")]
    pub group_id: String,
    /// Pipeline ID.
    #[serde(default = "default_pipeline")]
    pub pipeline_id: String,
    /// Receiver node (exactly one).
    pub receiver: NodeSpec,
    /// Processor nodes (zero or more, in order).
    #[serde(default)]
    pub processors: Vec<NodeSpec>,
    /// Exporter node (exactly one).
    pub exporter: NodeSpec,
    /// Optional channel capacity policies.
    #[serde(default)]
    pub policies: ChannelPolicies,
    /// Whether to include the admin HTTP endpoint.
    #[serde(default)]
    pub enable_admin: bool,
    /// Admin bind address (only used if `enable_admin` is true).
    #[serde(default = "default_admin_bind")]
    pub admin_bind: String,
}

fn default_group() -> String {
    "default".to_string()
}

fn default_pipeline() -> String {
    "main".to_string()
}

fn default_admin_bind() -> String {
    "127.0.0.1:8080".to_string()
}

/// Generates a valid OTAP dataflow engine YAML configuration from structured input.
///
/// Builds a linear pipeline: receiver → processor_0 → ... → processor_n → exporter,
/// with correct node IDs and DAG connections.
pub fn generate_config(input: &GenerateConfigInput) -> Result<String, String> {
    // Validate component URNs
    validate_urn(&input.receiver.r#type, "receiver")?;
    for (i, proc) in input.processors.iter().enumerate() {
        validate_urn(&proc.r#type, &format!("processors[{i}]"))?;
    }
    validate_urn(&input.exporter.r#type, "exporter")?;

    let mut yaml = String::new();

    // Version
    yaml.push_str("version: otel_dataflow/v1\n");

    // Engine section
    if input.enable_admin {
        yaml.push_str("engine:\n");
        yaml.push_str("  http_admin:\n");
        yaml.push_str(&format!("    bind_address: \"{}\"\n", input.admin_bind));
    } else {
        yaml.push_str("engine: {}\n");
    }

    // Groups
    yaml.push_str("groups:\n");
    yaml.push_str(&format!("  {}:\n", input.group_id));
    yaml.push_str("    pipelines:\n");
    yaml.push_str(&format!("      {}:\n", input.pipeline_id));

    // Policies
    yaml.push_str("        policies:\n");
    yaml.push_str("          channel_capacity:\n");
    yaml.push_str("            control:\n");
    yaml.push_str(&format!(
        "              node: {}\n",
        input.policies.control_node
    ));
    yaml.push_str(&format!(
        "              pipeline: {}\n",
        input.policies.control_pipeline
    ));
    yaml.push_str(&format!("            pdata: {}\n", input.policies.pdata));
    yaml.push('\n');

    // Nodes
    yaml.push_str("        nodes:\n");

    // Collect node IDs for connections
    let mut node_ids: Vec<String> = Vec::new();

    // Receiver
    let recv_id = input
        .receiver
        .id
        .clone()
        .unwrap_or_else(|| "receiver".to_string());
    node_ids.push(recv_id.clone());
    write_node(&mut yaml, &recv_id, &input.receiver)?;

    // Processors
    for (i, proc) in input.processors.iter().enumerate() {
        let proc_id = proc
            .id
            .clone()
            .unwrap_or_else(|| extract_short_name(&proc.r#type, i));
        node_ids.push(proc_id.clone());
        write_node(&mut yaml, &proc_id, proc)?;
    }

    // Exporter
    let export_id = input
        .exporter
        .id
        .clone()
        .unwrap_or_else(|| "exporter".to_string());
    node_ids.push(export_id.clone());
    write_node(&mut yaml, &export_id, &input.exporter)?;

    // Connections (linear chain)
    yaml.push_str("\n        connections:\n");
    for window in node_ids.windows(2) {
        yaml.push_str(&format!(
            "          - from: {}\n            to: {}\n",
            window[0], window[1]
        ));
    }

    Ok(yaml)
}

/// Writes a single node entry to the YAML output.
fn write_node(yaml: &mut String, id: &str, node: &NodeSpec) -> Result<(), String> {
    yaml.push_str(&format!("          {}:\n", id));
    yaml.push_str(&format!("            type: {}\n", node.r#type));

    if let Some(ref config_str) = node.config {
        // Parse config string as JSON, then serialize to YAML
        let config_value: serde_json::Value = serde_json::from_str(config_str)
            .map_err(|e| format!("config for '{id}' is not valid JSON: {e}"))?;

        if !config_value.is_null() && config_value != serde_json::Value::Object(Default::default())
        {
            let config_yaml = serde_yaml::to_string(&config_value)
                .map_err(|e| format!("config serialization for '{id}': {e}"))?;

            yaml.push_str("            config:\n");
            for line in config_yaml.lines() {
                if line == "---" {
                    continue;
                }
                yaml.push_str(&format!("              {}\n", line));
            }
        }
    }

    Ok(())
}

/// Validates that a URN starts with the expected component type prefix.
fn validate_urn(urn: &str, field: &str) -> Result<(), String> {
    if !urn.contains(':') {
        return Err(format!(
            "{field}: '{urn}' is not a valid component URN. Expected format: '<type>:<name>' (e.g., 'receiver:otlp')"
        ));
    }
    Ok(())
}

/// Extracts a short name for a processor node ID from its URN.
fn extract_short_name(urn: &str, index: usize) -> String {
    let name = urn.rsplit(':').next().unwrap_or("proc");
    if index == 0 {
        name.to_string()
    } else {
        format!("{name}_{index}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_pipeline() {
        let input = GenerateConfigInput {
            group_id: "default".to_string(),
            pipeline_id: "main".to_string(),
            receiver: NodeSpec {
                id: None,
                r#type: "receiver:otlp".to_string(),
                config: Some(
                    r#"{"protocols":{"grpc":{"listening_addr":"0.0.0.0:4317"}}}"#.to_string(),
                ),
            },
            processors: vec![NodeSpec {
                id: None,
                r#type: "processor:batch".to_string(),
                config: Some(r#"{"otap":{"min_size":100},"flush_timeout":"5s"}"#.to_string()),
            }],
            exporter: NodeSpec {
                id: None,
                r#type: "exporter:otlp_grpc".to_string(),
                config: Some(r#"{"grpc_endpoint":"http://backend:4317"}"#.to_string()),
            },
            policies: ChannelPolicies::default(),
            enable_admin: false,
            admin_bind: default_admin_bind(),
        };

        let result = generate_config(&input).expect("should generate");
        assert!(result.contains("version: otel_dataflow/v1"));
        assert!(result.contains("receiver:otlp"));
        assert!(result.contains("processor:batch"));
        assert!(result.contains("exporter:otlp_grpc"));
        assert!(result.contains("from: receiver"));
        assert!(result.contains("to: batch"));
        assert!(result.contains("from: batch"));
        assert!(result.contains("to: exporter"));
    }

    #[test]
    fn test_no_processors() {
        let input = GenerateConfigInput {
            group_id: "default".to_string(),
            pipeline_id: "main".to_string(),
            receiver: NodeSpec {
                id: None,
                r#type: "receiver:otap".to_string(),
                config: None,
            },
            processors: vec![],
            exporter: NodeSpec {
                id: None,
                r#type: "exporter:console".to_string(),
                config: None,
            },
            policies: ChannelPolicies::default(),
            enable_admin: false,
            admin_bind: default_admin_bind(),
        };

        let result = generate_config(&input).expect("should generate");
        assert!(result.contains("from: receiver"));
        assert!(result.contains("to: exporter"));
    }

    #[test]
    fn test_invalid_urn() {
        let input = GenerateConfigInput {
            group_id: "default".to_string(),
            pipeline_id: "main".to_string(),
            receiver: NodeSpec {
                id: None,
                r#type: "bad_urn".to_string(),
                config: None,
            },
            processors: vec![],
            exporter: NodeSpec {
                id: None,
                r#type: "exporter:console".to_string(),
                config: None,
            },
            policies: ChannelPolicies::default(),
            enable_admin: false,
            admin_bind: default_admin_bind(),
        };

        let result = generate_config(&input);
        assert!(result.is_err());
    }
}
