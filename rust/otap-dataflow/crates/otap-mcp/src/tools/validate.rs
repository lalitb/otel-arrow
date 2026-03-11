// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

//! Configuration validation tool.
//!
//! Parses and validates OTAP dataflow engine YAML configurations,
//! returning detailed error messages on failure.

use otap_df_config::engine::OtelDataflowSpec;
use otap_df_config::node::NodeKind;
use otap_df_otap::OTAP_PIPELINE_FACTORY;
use serde::Serialize;
use std::borrow::Borrow;

const OBSERVABILITY_GROUP_ID: &str = "__system";
const OBSERVABILITY_PIPELINE_ID: &str = "__observability";

/// Result of a configuration validation.
#[derive(Debug, Clone, Serialize)]
pub struct ValidationResult {
    /// Whether the configuration is valid.
    pub valid: bool,
    /// Validation messages (errors and warnings).
    pub messages: Vec<ValidationMessage>,
    /// Summary counts.
    pub summary: ValidationSummary,
}

/// A single validation message.
#[derive(Debug, Clone, Serialize)]
pub struct ValidationMessage {
    /// Severity level.
    pub level: MessageLevel,
    /// Human-readable message.
    pub message: String,
}

/// Severity of a validation message.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageLevel {
    /// A critical error that prevents the config from being used.
    Error,
    /// A non-fatal warning.
    Warning,
    /// Informational message.
    Info,
}

/// Summary counts for validation results.
#[derive(Debug, Clone, Serialize)]
pub struct ValidationSummary {
    /// Number of errors.
    pub errors: usize,
    /// Number of warnings.
    pub warnings: usize,
    /// Number of pipeline groups found.
    pub pipeline_groups: usize,
    /// Number of pipelines found.
    pub pipelines: usize,
    /// Number of nodes found.
    pub nodes: usize,
}

/// Validates an OTAP dataflow engine configuration string.
///
/// Performs three levels of validation:
/// 1. YAML parsing (syntax)
/// 2. Schema validation (structure)
/// 3. Component existence checks (all node URNs map to registered components)
pub fn validate_config(yaml_content: &str) -> ValidationResult {
    let mut messages = Vec::new();
    let mut pipeline_groups = 0;
    let mut pipelines = 0;
    let mut nodes = 0;

    // Phase 1: Parse the YAML
    let spec = match OtelDataflowSpec::from_yaml(yaml_content) {
        Ok(spec) => spec,
        Err(e) => {
            messages.push(ValidationMessage {
                level: MessageLevel::Error,
                message: format!("YAML parse error: {e}"),
            });
            return ValidationResult {
                valid: false,
                messages,
                summary: ValidationSummary {
                    errors: 1,
                    warnings: 0,
                    pipeline_groups: 0,
                    pipelines: 0,
                    nodes: 0,
                },
            };
        }
    };

    // Phase 2: Structural validation
    if let Err(e) = spec.validate() {
        messages.push(ValidationMessage {
            level: MessageLevel::Error,
            message: format!("Validation error: {e}"),
        });
    }

    // Phase 3: Component existence and per-component config validation.
    // NodeUrn.as_str() returns canonical form "urn:otel:<kind>:<id>",
    // which matches the factory map keys.
    let receiver_map = OTAP_PIPELINE_FACTORY.get_receiver_factory_map();
    let processor_map = OTAP_PIPELINE_FACTORY.get_processor_factory_map();
    let exporter_map = OTAP_PIPELINE_FACTORY.get_exporter_factory_map();

    for (group_id, group) in &spec.groups {
        pipeline_groups += 1;
        for (pipeline_id, pipeline) in &group.pipelines {
            pipelines += 1;
            validate_pipeline_nodes(
                &mut messages,
                &mut nodes,
                group_id,
                pipeline_id,
                pipeline.node_iter(),
                receiver_map,
                processor_map,
                exporter_map,
            );
        }
    }

    if let Some(observability) = &spec.engine.observability.pipeline {
        pipeline_groups += 1;
        pipelines += 1;
        validate_pipeline_nodes(
            &mut messages,
            &mut nodes,
            OBSERVABILITY_GROUP_ID,
            OBSERVABILITY_PIPELINE_ID,
            observability.nodes.iter(),
            receiver_map,
            processor_map,
            exporter_map,
        );
    }

    let errors = messages
        .iter()
        .filter(|m| matches!(m.level, MessageLevel::Error))
        .count();
    let warnings = messages
        .iter()
        .filter(|m| matches!(m.level, MessageLevel::Warning))
        .count();

    ValidationResult {
        valid: errors == 0,
        messages,
        summary: ValidationSummary {
            errors,
            warnings,
            pipeline_groups,
            pipelines,
            nodes,
        },
    }
}

fn validate_pipeline_nodes<'a, I>(
    messages: &mut Vec<ValidationMessage>,
    nodes: &mut usize,
    group_id: &str,
    pipeline_id: &str,
    pipeline_nodes: I,
    receiver_map: &std::collections::HashMap<
        &'static str,
        otap_df_engine::ReceiverFactory<otap_df_otap::pdata::OtapPdata>,
    >,
    processor_map: &std::collections::HashMap<
        &'static str,
        otap_df_engine::ProcessorFactory<otap_df_otap::pdata::OtapPdata>,
    >,
    exporter_map: &std::collections::HashMap<
        &'static str,
        otap_df_engine::ExporterFactory<otap_df_otap::pdata::OtapPdata>,
    >,
) where
    I: IntoIterator<
        Item = (
            &'a std::borrow::Cow<'static, str>,
            &'a std::sync::Arc<otap_df_config::node::NodeUserConfig>,
        ),
    >,
{
    for (node_id, node) in pipeline_nodes {
        *nodes += 1;
        let node_id = node_id.as_ref();
        let node: &otap_df_config::node::NodeUserConfig = node.borrow();
        let urn = node.r#type.as_str();
        let kind = node.r#type.kind();

        match kind {
            NodeKind::Receiver => {
                if let Some(factory) = receiver_map.get(urn) {
                    if let Err(e) = (factory.validate_config)(&node.config) {
                        messages.push(ValidationMessage {
                            level: MessageLevel::Error,
                            message: format!(
                                "Node '{node_id}' in {group_id}/{pipeline_id}: \
                                 config error for '{urn}': {e}"
                            ),
                        });
                    }
                } else {
                    messages.push(ValidationMessage {
                        level: MessageLevel::Error,
                        message: format!(
                            "Node '{node_id}' in {group_id}/{pipeline_id}: \
                             receiver '{urn}' is not registered in this build"
                        ),
                    });
                }
            }
            NodeKind::Processor | NodeKind::ProcessorChain => {
                if let Some(factory) = processor_map.get(urn) {
                    if let Err(e) = (factory.validate_config)(&node.config) {
                        messages.push(ValidationMessage {
                            level: MessageLevel::Error,
                            message: format!(
                                "Node '{node_id}' in {group_id}/{pipeline_id}: \
                                 config error for '{urn}': {e}"
                            ),
                        });
                    }
                } else {
                    messages.push(ValidationMessage {
                        level: MessageLevel::Error,
                        message: format!(
                            "Node '{node_id}' in {group_id}/{pipeline_id}: \
                             processor '{urn}' is not registered in this build"
                        ),
                    });
                }
            }
            NodeKind::Exporter => {
                if let Some(factory) = exporter_map.get(urn) {
                    if let Err(e) = (factory.validate_config)(&node.config) {
                        messages.push(ValidationMessage {
                            level: MessageLevel::Error,
                            message: format!(
                                "Node '{node_id}' in {group_id}/{pipeline_id}: \
                                 config error for '{urn}': {e}"
                            ),
                        });
                    }
                } else {
                    messages.push(ValidationMessage {
                        level: MessageLevel::Error,
                        message: format!(
                            "Node '{node_id}' in {group_id}/{pipeline_id}: \
                             exporter '{urn}' is not registered in this build"
                        ),
                    });
                }
            }
        }
    }
}
